// src/rx.rs (: metrics)
use crate::metrics;
use crate::parser::SeqExtractor;
use crate::pool::{PacketPool, Pkt};
use crate::util::{now_nanos, spin_wait};
use anyhow::Context;
use crossbeam::queue::ArrayQueue;
use log::debug;
use nix::errno::Errno;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use bytes::BytesMut;
#[cfg(target_os = "linux")]
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned};
#[cfg(target_os = "linux")]
use std::io::IoSliceMut;

pub fn rx_loop(
    chan_name: &str,
    sock: &UdpSocket,
    seq: Arc<dyn SeqExtractor>,
    q_out: Arc<ArrayQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<crate::util::BarrierFlag>,
    spin_loops_per_yield: u32,
    rx_batch: usize,
    timestamping: bool,
) -> anyhow::Result<()> {
    let fd = sock.as_raw_fd();
    let mut dropped: u64 = 0;
    let chan_id = if chan_name == "A" { b'A' } else { b'B' };

    sock.set_nonblocking(true).context("set nonblocking")?;

    let batch = rx_batch.max(1);
    #[cfg(target_os = "linux")]
    let use_recvmmsg: bool = !timestamping && batch > 1;
    #[cfg(not(target_os = "linux"))]
    let use_recvmmsg: bool = false;

    loop {
        if shutdown.is_raised() { break; }

        let mut progressed = false;

        // Cache a single now_nanos() per loop when timestamping is off
        let mut loop_now_cache: Option<u64> = None;
        if !timestamping { loop_now_cache = Some(now_nanos()); }

        if use_recvmmsg {
            #[cfg(target_os = "linux")]
            unsafe {
                // Prepare buffers and mmsghdrs for batched receive
                let mut bufs = Vec::with_capacity(batch);
                let mut iovecs: Vec<libc::iovec> = Vec::with_capacity(batch);
                let mut hdrs: Vec<libc::mmsghdr> = Vec::with_capacity(batch);

                for _ in 0..batch {
                    let mut b = pool.get();
                    let s = b.chunk_mut();
                    let iov = libc::iovec {
                        iov_base: s.as_mut_ptr() as *mut libc::c_void,
                        iov_len: s.len(),
                    };
                    iovecs.push(iov);
                    bufs.push(b);
                }

                for i in 0..batch {
                    let mut msghdr: libc::msghdr = std::mem::zeroed();
                    msghdr.msg_name = std::ptr::null_mut();
                    msghdr.msg_namelen = 0;
                    msghdr.msg_iov = &mut iovecs[i] as *mut libc::iovec;
                    msghdr.msg_iovlen = 1;
                    msghdr.msg_control = std::ptr::null_mut();
                    msghdr.msg_controllen = 0;
                    msghdr.msg_flags = 0;
                    let mmsg = libc::mmsghdr { msg_hdr: msghdr, msg_len: 0 };
                    hdrs.push(mmsg);
                }

                let ret = libc::recvmmsg(
                    fd,
                    hdrs.as_mut_ptr(),
                    batch as u32,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(),
                );

                if ret < 0 {
                    let err = Errno::last();
                    if err == Errno::EAGAIN || err == Errno::EWOULDBLOCK || err == Errno::EINTR {
                        // no progress
                    } else {
                        return Err(anyhow::anyhow!("recvmmsg error: {}", std::io::Error::from(err)));
                    }
                } else if ret > 0 {
                    progressed = true;
                    let ts = loop_now_cache.unwrap_or_else(now_nanos);
                    let count = ret as usize;
                    for i in 0..count {
                        let n = hdrs[i].msg_len as usize;
                        let mut buf = std::mem::replace(&mut bufs[i], BytesMut::new());
                        buf.advance_mut(n);
                        let maybe_seq = seq.extract_seq(&buf);
                        if let Some(sv) = maybe_seq {
                            let pkt = Pkt { buf, len: n, seq: sv, ts_nanos: ts, chan: chan_id };
                            if let Err(_full) = q_out.push(pkt) {
                                dropped += 1;
                                metrics::inc_rx_drop(chan_name);
                                if dropped % 10_000 == 1 {
                                    debug!("{}_rx: queue full, dropped={}", chan_name, dropped);
                                }
                            } else {
                                metrics::inc_rx(chan_name, n);
                            }
                        } else {
                            pool.put(buf);
                        }
                    }
                    // Return unused buffers to pool
                    for j in count..batch {
                        let b = std::mem::replace(&mut bufs[j], BytesMut::new());
                        pool.put(b);
                    }
                } else {
                    // ret == 0 unlikely for DONTWAIT but handle conservatively
                }
            }
        } else {
            // Per-packet path (recv/recvmsg)
            for _ in 0..batch {
                if shutdown.is_raised() { break; }
                let mut buf = pool.get();
                let dst = unsafe {
                    let s = buf.chunk_mut();
                    std::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, s.len())
                };

                let res_len_ts = if timestamping {
                    #[cfg(target_os = "linux")]
                    {
                        let mut iov = [IoSliceMut::new(dst)];
                        let mut cmsg_buf = nix::cmsg_space!([libc::timespec; 3]);
                        match recvmsg(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::MSG_DONTWAIT) {
                            Ok(msg) => {
                                let mut ts_nanos: u64 = 0;
                                for c in msg.cmsgs() {
                                    match c {
                                        ControlMessageOwned::ScmTimestampns(ts) => {
                                            ts_nanos = (ts.tv_sec() as u64) * 1_000_000_000 + (ts.tv_nsec() as u64);
                                        }
                                        ControlMessageOwned::ScmTimestamping(tss) => {
                                            let pick = tss.iter().rev().find(|t| t.tv_sec() != 0 || t.tv_nsec() != 0).copied();
                                            if let Some(tv) = pick {
                                                ts_nanos = (tv.tv_sec() as u64) * 1_000_000_000 + (tv.tv_nsec() as u64);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                if ts_nanos == 0 {
                                    // Fallback only if timestamp not present; cache once per loop
                                    let fallback = if let Some(v) = loop_now_cache { v } else { let v = now_nanos(); loop_now_cache = Some(v); v };
                                    if msg.bytes > 0 { Ok((msg.bytes, fallback)) } else { Err(Errno::EAGAIN) }
                                } else {
                                    if msg.bytes > 0 { Ok((msg.bytes, ts_nanos)) } else { Err(Errno::EAGAIN) }
                                }
                            }
                            Err(nix::Error::Sys(e)) => Err(e),
                            Err(_) => Err(Errno::EAGAIN),
                        }
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        unsafe {
                            let n = libc::recv(fd, dst.as_ptr() as *mut libc::c_void, dst.len(), libc::MSG_DONTWAIT);
                            if n >= 0 { Ok((n as usize, now_nanos())) } else { Err(Errno::last()) }
                        }
                    }
                } else {
                    unsafe {
                        let n = libc::recv(fd, dst.as_ptr() as *mut libc::c_void, dst.len(), libc::MSG_DONTWAIT);
                        if n >= 0 { Ok((n as usize, loop_now_cache.unwrap())) } else { Err(Errno::last()) }
                    }
                };

                match res_len_ts {
                    Ok((n, ts)) => {
                        unsafe { buf.advance_mut(n); }
                        let maybe_seq = seq.extract_seq(&buf);
                        if let Some(sv) = maybe_seq {
                            let pkt = Pkt { buf, len: n, seq: sv, ts_nanos: ts, chan: chan_id };
                            if let Err(_full) = q_out.push(pkt) {
                                dropped += 1;
                                metrics::inc_rx_drop(chan_name);
                                if dropped % 10_000 == 1 {
                                    debug!("{}_rx: queue full, dropped={}", chan_name, dropped);
                                }
                            } else {
                                metrics::inc_rx(chan_name, n);
                            }
                        } else {
                            pool.put(buf);
                        }
                        progressed = true;
                    }
                    Err(err) => {
                        if err == Errno::EAGAIN || err == Errno::EWOULDBLOCK || err == Errno::EINTR {
                            break;
                        } else {
                            return Err(anyhow::anyhow!("recv error: {}", std::io::Error::from(err)));
                        }
                    }
                }
            }
        }

        if !progressed {
            spin_wait(spin_loops_per_yield);
        }
    }

    Ok(())
}