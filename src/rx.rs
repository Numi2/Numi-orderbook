// src/rx.rs (: metrics)
use crate::config::TimestampingMode;
use crate::metrics;
use crate::parser::SeqExtractor;
use crate::pool::{PacketPool, Pkt, PktBuf, TsKind};
use crate::spsc::{AdaptiveBatchCap, SpscQueue, DEFAULT_BATCH_CAP};
use crate::util::now_nanos;
use anyhow::Context;
use bytes::BufMut;
#[cfg(target_os = "linux")]
use bytes::BytesMut;
use log::debug;
use nix::errno::Errno;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::sync::Arc;

pub struct RxConfig {
    pub spin_loops_per_yield: u32,
    pub rx_batch: usize,
    pub ts_mode: Option<TimestampingMode>,
}

pub fn rx_loop(
    chan_name: &str,
    sock: &UdpSocket,
    seq: Arc<dyn SeqExtractor>,
    q_out: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<crate::util::BarrierFlag>,
    cfg: RxConfig,
) -> anyhow::Result<()> {
    let RxConfig {
        spin_loops_per_yield,
        rx_batch,
        ts_mode,
    } = cfg;
    let fd = sock.as_raw_fd();
    let mut dropped: u64 = 0;
    let chan_id = if chan_name == "A" { b'A' } else { b'B' };

    sock.set_nonblocking(true).context("set nonblocking")?;

    let batch = rx_batch.max(1);
    let ts_off = ts_mode
        .as_ref()
        .map(|m| matches!(m, TimestampingMode::Off))
        .unwrap_or(true);
    #[cfg(target_os = "linux")]
    let use_recvmmsg: bool = batch > 1;
    #[cfg(not(target_os = "linux"))]
    let use_recvmmsg: bool = false;
    #[cfg(target_os = "linux")]
    let wants_timestamps = !ts_off;

    // Preallocate vectors for recvmmsg path to avoid per-iteration allocations
    #[cfg(target_os = "linux")]
    let mut bufs: Vec<BytesMut> = if use_recvmmsg {
        (0..batch).map(|_| BytesMut::new()).collect()
    } else {
        Vec::new()
    };
    #[cfg(target_os = "linux")]
    let mut iovecs: Vec<libc::iovec> = if use_recvmmsg {
        (0..batch)
            .map(|_| libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            })
            .collect()
    } else {
        Vec::new()
    };
    #[cfg(target_os = "linux")]
    let mut hdrs: Vec<libc::mmsghdr> = if use_recvmmsg {
        let mut v = Vec::with_capacity(batch);
        for iovec in iovecs.iter_mut().take(batch) {
            let mut mh: libc::msghdr = unsafe { std::mem::zeroed() };
            mh.msg_name = std::ptr::null_mut();
            mh.msg_namelen = 0;
            mh.msg_iov = iovec as *mut libc::iovec;
            mh.msg_iovlen = 1;
            mh.msg_control = std::ptr::null_mut();
            mh.msg_controllen = 0;
            mh.msg_flags = 0;
            v.push(libc::mmsghdr {
                msg_hdr: mh,
                msg_len: 0,
            });
        }
        v
    } else {
        Vec::new()
    };
    #[cfg(target_os = "linux")]
    let mut cmsgs: Vec<TimestampCmsgBuffer> = if use_recvmmsg && wants_timestamps {
        (0..batch).map(|_| TimestampCmsgBuffer::new()).collect()
    } else {
        Vec::new()
    };
    let ring_batch_limit = batch.min(DEFAULT_BATCH_CAP).min(q_out.capacity());
    let mut ring_batch_cap = AdaptiveBatchCap::new(1, ring_batch_limit);
    let mut pending_pkts: Vec<Pkt> = Vec::with_capacity(ring_batch_cap.max());

    let queue_label: &'static str = if chan_name == "A" { "rx_a" } else { "rx_b" };
    let mut iter: u64 = 0;
    let mut idle_iters: u32 = 0;
    loop {
        if shutdown.is_raised() {
            break;
        }

        let mut progressed = false;

        // Cache a single now_nanos() per loop when timestamping is off
        let mut loop_now_cache: Option<u64> = None;
        if ts_off {
            loop_now_cache = Some(now_nanos());
        }

        if use_recvmmsg {
            #[cfg(target_os = "linux")]
            unsafe {
                // Prepare buffers and update iovecs in-place
                for i in 0..batch {
                    bufs[i] = pool.get();
                    let s = bufs[i].chunk_mut();
                    iovecs[i].iov_base = s.as_mut_ptr() as *mut libc::c_void;
                    iovecs[i].iov_len = s.len();
                    hdrs[i].msg_hdr.msg_iov = &mut iovecs[i] as *mut libc::iovec;
                    hdrs[i].msg_hdr.msg_iovlen = 1;
                    hdrs[i].msg_hdr.msg_flags = 0;
                    if wants_timestamps {
                        hdrs[i].msg_hdr.msg_control = cmsgs[i].as_mut_ptr() as *mut libc::c_void;
                        hdrs[i].msg_hdr.msg_controllen = cmsgs[i].len();
                    } else {
                        hdrs[i].msg_hdr.msg_control = std::ptr::null_mut();
                        hdrs[i].msg_hdr.msg_controllen = 0;
                    }
                    hdrs[i].msg_len = 0;
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
                        recycle_prepared_buffers(&mut bufs, &pool);
                        // no progress
                    } else {
                        recycle_prepared_buffers(&mut bufs, &pool);
                        return Err(anyhow::anyhow!(
                            "recvmmsg error: {}",
                            std::io::Error::from(err)
                        ));
                    }
                } else if ret > 0 {
                    progressed = true;
                    let count = ret as usize;
                    for i in 0..count {
                        let n = hdrs[i].msg_len as usize;
                        let mut buf = std::mem::take(&mut bufs[i]);
                        buf.advance_mut(n);
                        let maybe_seq = seq.extract_seq(&buf);
                        if let Some(sv) = maybe_seq {
                            let rx_ts = packet_timestamp(
                                wants_timestamps.then_some(&hdrs[i].msg_hdr),
                                &mut loop_now_cache,
                            );
                            let pkt = Pkt {
                                buf: PktBuf::Bytes(buf),
                                len: n,
                                seq: sv,
                                ts_nanos: rx_ts.nanos,
                                chan: chan_id,
                                _ts_kind: rx_ts.kind,
                                merge_emit_ns: 0,
                            };
                            stage_or_flush_rx_packet(
                                chan_name,
                                &q_out,
                                &pool,
                                pkt,
                                &mut pending_pkts,
                                &mut ring_batch_cap,
                                &mut dropped,
                            );
                        } else {
                            pool.put(buf);
                        }
                    }
                    // Return unused buffers to pool
                    for buf in bufs.iter_mut().take(batch).skip(count) {
                        let b = std::mem::take(buf);
                        if b.capacity() > 0 {
                            pool.put(b);
                        }
                    }
                } else {
                    // ret == 0 unlikely for DONTWAIT but handle conservatively
                    recycle_prepared_buffers(&mut bufs, &pool);
                }
            }
        } else {
            // Per-packet path (recv/recvmsg)
            for _ in 0..batch {
                if shutdown.is_raised() {
                    break;
                }
                let mut buf = pool.get();
                let dst = unsafe {
                    let s = buf.chunk_mut();
                    std::slice::from_raw_parts_mut(s.as_mut_ptr(), s.len())
                };

                let res_len_ts = if !ts_off {
                    #[cfg(target_os = "linux")]
                    {
                        recvmsg_one(fd, dst, &mut loop_now_cache)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        unsafe {
                            let n = libc::recv(
                                fd,
                                dst.as_ptr() as *mut libc::c_void,
                                dst.len(),
                                libc::MSG_DONTWAIT,
                            );
                            if n >= 0 {
                                Ok((n as usize, now_nanos(), TsKind::Sw))
                            } else {
                                Err(Errno::last())
                            }
                        }
                    }
                } else {
                    unsafe {
                        let n = libc::recv(
                            fd,
                            dst.as_ptr() as *mut libc::c_void,
                            dst.len(),
                            libc::MSG_DONTWAIT,
                        );
                        if n >= 0 {
                            Ok((n as usize, loop_now_cache.unwrap(), TsKind::Sw))
                        } else {
                            Err(Errno::last())
                        }
                    }
                };

                match res_len_ts {
                    Ok((n, ts, kind)) => {
                        unsafe {
                            buf.advance_mut(n);
                        }
                        let maybe_seq = seq.extract_seq(&buf);
                        if let Some(sv) = maybe_seq {
                            let pkt = Pkt {
                                buf: PktBuf::Bytes(buf),
                                len: n,
                                seq: sv,
                                ts_nanos: ts,
                                chan: chan_id,
                                _ts_kind: kind,
                                merge_emit_ns: 0,
                            };
                            stage_or_flush_rx_packet(
                                chan_name,
                                &q_out,
                                &pool,
                                pkt,
                                &mut pending_pkts,
                                &mut ring_batch_cap,
                                &mut dropped,
                            );
                        } else {
                            pool.put(buf);
                        }
                        progressed = true;
                    }
                    Err(err) => {
                        if err == Errno::EAGAIN || err == Errno::EWOULDBLOCK || err == Errno::EINTR
                        {
                            pool.put(buf);
                            break;
                        } else {
                            pool.put(buf);
                            flush_rx_packet_batch(
                                chan_name,
                                &q_out,
                                &pool,
                                &mut pending_pkts,
                                &mut ring_batch_cap,
                                &mut dropped,
                            );
                            return Err(anyhow::anyhow!(
                                "recv error: {}",
                                std::io::Error::from(err)
                            ));
                        }
                    }
                }
            }
        }
        flush_rx_packet_batch(
            chan_name,
            &q_out,
            &pool,
            &mut pending_pkts,
            &mut ring_batch_cap,
            &mut dropped,
        );

        if !progressed {
            crate::util::adaptive_wait(&mut idle_iters, spin_loops_per_yield);
        } else {
            idle_iters = 0;
        }

        iter = iter.wrapping_add(1);
        if (iter & 0x3fff) == 0 {
            metrics::set_queue_len(queue_label, q_out.len());
        }
    }

    Ok(())
}

#[inline]
fn stage_or_flush_rx_packet(
    chan_name: &str,
    q_out: &SpscQueue<Pkt>,
    pool: &PacketPool,
    pkt: Pkt,
    pending: &mut Vec<Pkt>,
    batch_cap: &mut AdaptiveBatchCap,
    dropped: &mut u64,
) {
    pending.push(pkt);
    if pending.len() >= batch_cap.current() {
        flush_rx_packet_batch(chan_name, q_out, pool, pending, batch_cap, dropped);
    }
}

#[inline]
fn flush_rx_packet_batch(
    chan_name: &str,
    q_out: &SpscQueue<Pkt>,
    pool: &PacketPool,
    pending: &mut Vec<Pkt>,
    batch_cap: &mut AdaptiveBatchCap,
    dropped: &mut u64,
) -> usize {
    let attempted = pending.len().min(batch_cap.current());
    if attempted == 0 {
        return 0;
    }

    let writable = q_out.spare_capacity().min(attempted);
    let accepted_bytes = pending
        .iter()
        .take(writable)
        .map(|pkt| pkt.len)
        .sum::<usize>();
    let pushed = q_out.push_batch(pending, writable);
    debug_assert_eq!(pushed, writable);
    if pushed > 0 {
        metrics::inc_rx_batch(chan_name, pushed, accepted_bytes);
    }

    if pushed < attempted {
        let rejected = pending.len();
        for pkt in pending.drain(..) {
            pkt.recycle(pool);
        }
        record_rx_drops(chan_name, rejected, dropped);
        batch_cap.reset();
    } else {
        batch_cap.record(attempted, pushed);
    }

    pushed
}

#[inline]
fn record_rx_drops(chan_name: &str, count: usize, dropped: &mut u64) {
    if count == 0 {
        return;
    }

    let before = *dropped;
    *dropped = dropped.saturating_add(count as u64);
    metrics::inc_rx_drop_batch(chan_name, count);
    if before == 0 || before / 10_000 != *dropped / 10_000 {
        debug!("{}_rx: queue full, dropped={}", chan_name, *dropped);
    }
}

#[cfg(target_os = "linux")]
fn recycle_prepared_buffers(bufs: &mut [BytesMut], pool: &PacketPool) {
    for buf in bufs {
        let b = std::mem::take(buf);
        if b.capacity() > 0 {
            pool.put(b);
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RxTimestamp {
    nanos: u64,
    kind: TsKind,
}

#[cfg(target_os = "linux")]
#[inline]
fn cached_now(cache: &mut Option<u64>) -> u64 {
    if let Some(v) = *cache {
        v
    } else {
        let v = now_nanos();
        *cache = Some(v);
        v
    }
}

#[cfg(target_os = "linux")]
#[inline]
fn packet_timestamp(hdr: Option<&libc::msghdr>, cache: &mut Option<u64>) -> RxTimestamp {
    if let Some(hdr) = hdr {
        if let Some(ts) = unsafe { timestamp_from_cmsgs(hdr) } {
            return ts;
        }
    }

    RxTimestamp {
        nanos: cached_now(cache),
        kind: TsKind::Sw,
    }
}

#[cfg(target_os = "linux")]
const TIMESTAMP_CMSG_SPACE: usize = unsafe {
    libc::CMSG_SPACE(std::mem::size_of::<[libc::timespec; 3]>() as libc::c_uint) as usize
};

#[cfg(target_os = "linux")]
#[repr(C, align(16))]
struct TimestampCmsgBuffer {
    bytes: [u8; TIMESTAMP_CMSG_SPACE],
}

#[cfg(target_os = "linux")]
impl TimestampCmsgBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; TIMESTAMP_CMSG_SPACE],
        }
    }

    #[inline]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    #[inline]
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(target_os = "linux")]
fn recvmsg_one(
    fd: libc::c_int,
    dst: &mut [u8],
    cache: &mut Option<u64>,
) -> Result<(usize, u64, TsKind), Errno> {
    let mut iov = libc::iovec {
        iov_base: dst.as_mut_ptr() as *mut libc::c_void,
        iov_len: dst.len(),
    };
    let mut cmsg = TimestampCmsgBuffer::new();
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov as *mut libc::iovec;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = cmsg.len();

    let n = unsafe { libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(Errno::last());
    }
    if n == 0 {
        return Err(Errno::EAGAIN);
    }

    let ts = packet_timestamp(Some(&hdr), cache);
    Ok((n as usize, ts.nanos, ts.kind))
}

#[cfg(target_os = "linux")]
unsafe fn timestamp_from_cmsgs(hdr: &libc::msghdr) -> Option<RxTimestamp> {
    if hdr.msg_controllen == 0 || (hdr.msg_flags & libc::MSG_CTRUNC) != 0 {
        return None;
    }

    let hdr_ptr = hdr as *const libc::msghdr;
    let mut cmsg = libc::CMSG_FIRSTHDR(hdr_ptr);
    while !cmsg.is_null() {
        let level = (*cmsg).cmsg_level;
        let ty = (*cmsg).cmsg_type;
        if level == libc::SOL_SOCKET && ty == libc::SCM_TIMESTAMPNS {
            if cmsg_has_payload(cmsg, std::mem::size_of::<libc::timespec>()) {
                let tv = (libc::CMSG_DATA(cmsg) as *const libc::timespec).read_unaligned();
                if let Some(nanos) = timespec_to_nanos(tv) {
                    return Some(RxTimestamp {
                        nanos,
                        kind: TsKind::Sw,
                    });
                }
            }
        } else if level == libc::SOL_SOCKET
            && ty == libc::SCM_TIMESTAMPING
            && cmsg_has_payload(cmsg, std::mem::size_of::<[libc::timespec; 3]>())
        {
            let tv = libc::CMSG_DATA(cmsg) as *const libc::timespec;
            for idx in (0..3).rev() {
                let current = tv.add(idx).read_unaligned();
                if let Some(nanos) = timespec_to_nanos(current) {
                    let kind = match idx {
                        2 => TsKind::HwRaw,
                        1 => TsKind::HwSys,
                        _ => TsKind::Sw,
                    };
                    return Some(RxTimestamp { nanos, kind });
                }
            }
        }
        cmsg = libc::CMSG_NXTHDR(hdr_ptr, cmsg);
    }

    None
}

#[cfg(target_os = "linux")]
#[inline]
unsafe fn cmsg_has_payload(cmsg: *const libc::cmsghdr, payload_len: usize) -> bool {
    let required = libc::CMSG_LEN(payload_len as libc::c_uint) as usize;
    (*cmsg).cmsg_len >= required
}

#[cfg(target_os = "linux")]
#[inline]
fn timespec_to_nanos(tv: libc::timespec) -> Option<u64> {
    if tv.tv_sec < 0 || tv.tv_nsec < 0 {
        return None;
    }
    let nanos = (tv.tv_sec as u64)
        .checked_mul(1_000_000_000)?
        .checked_add(tv.tv_nsec as u64)?;
    (nanos != 0).then_some(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn pkt_from_pool(pool: &PacketPool, seq: u64) -> Pkt {
        Pkt {
            buf: PktBuf::Bytes(pool.get()),
            len: 0,
            seq,
            ts_nanos: 0,
            chan: b'A',
            _ts_kind: TsKind::Sw,
            merge_emit_ns: 0,
        }
    }

    fn dummy_pkt(seq: u64) -> Pkt {
        Pkt {
            buf: PktBuf::Bytes(BytesMut::new()),
            len: 0,
            seq,
            ts_nanos: 0,
            chan: b'A',
            _ts_kind: TsKind::Sw,
            merge_emit_ns: 0,
        }
    }

    #[test]
    fn queue_full_recycles_rejected_packet_buffer() {
        let pool = PacketPool::new(1, 64).unwrap();
        let q = SpscQueue::new(2);
        q.push(dummy_pkt(1)).unwrap();
        q.push(dummy_pkt(2)).unwrap();

        let pkt = pkt_from_pool(&pool, 3);
        assert_eq!(pool.available(), 0);

        let mut dropped = 0;
        let mut pending = Vec::new();
        let mut batch_cap = AdaptiveBatchCap::new(1, DEFAULT_BATCH_CAP);
        stage_or_flush_rx_packet(
            "A",
            &q,
            &pool,
            pkt,
            &mut pending,
            &mut batch_cap,
            &mut dropped,
        );

        assert_eq!(dropped, 1);
        assert_eq!(pool.available(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn packet_batch_recycles_rejected_suffix() {
        let pool = PacketPool::new(3, 64).unwrap();
        let q = SpscQueue::new(2);
        let mut batch_cap = AdaptiveBatchCap::new(4, 4);
        let mut pending = vec![
            pkt_from_pool(&pool, 1),
            pkt_from_pool(&pool, 2),
            pkt_from_pool(&pool, 3),
        ];
        assert_eq!(pool.available(), 0);

        let mut dropped = 0;
        assert_eq!(
            flush_rx_packet_batch("A", &q, &pool, &mut pending, &mut batch_cap, &mut dropped),
            2
        );

        assert!(pending.is_empty());
        assert_eq!(q.len(), 2);
        assert_eq!(dropped, 1);
        assert_eq!(pool.available(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_recvmmsg_buffers_are_recycled_on_no_progress() {
        let pool = PacketPool::new(3, 64).unwrap();
        let mut bufs = vec![pool.get(), pool.get(), pool.get()];
        assert_eq!(pool.available(), 0);

        recycle_prepared_buffers(&mut bufs, &pool);

        assert_eq!(pool.available(), 3);
        assert!(bufs.iter().all(|buf| buf.capacity() == 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timestamping_cmsg_uses_actual_timestamp_slot_kind() {
        let tss = [
            libc::timespec {
                tv_sec: 11,
                tv_nsec: 7,
            },
            libc::timespec {
                tv_sec: 12,
                tv_nsec: 8,
            },
            libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        ];
        let ts = timestamp_from_test_cmsg(libc::SCM_TIMESTAMPING, &tss).unwrap();

        assert_eq!(ts.kind, TsKind::HwSys);
        assert_eq!(ts.nanos, 12_000_000_008);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timestamping_cmsg_uses_software_when_hardware_slots_are_empty() {
        let tss = [
            libc::timespec {
                tv_sec: 11,
                tv_nsec: 7,
            },
            libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        ];
        let ts = timestamp_from_test_cmsg(libc::SCM_TIMESTAMPING, &tss).unwrap();

        assert_eq!(ts.kind, TsKind::Sw);
        assert_eq!(ts.nanos, 11_000_000_007);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timestampns_cmsg_reports_software_timestamp() {
        let ts = libc::timespec {
            tv_sec: 13,
            tv_nsec: 9,
        };
        let parsed = timestampns_from_test_cmsg(ts).unwrap();

        assert_eq!(parsed.kind, TsKind::Sw);
        assert_eq!(parsed.nanos, 13_000_000_009);
    }

    #[cfg(target_os = "linux")]
    unsafe fn empty_msghdr_for_cmsg(buf: &mut TimestampCmsgBuffer) -> libc::msghdr {
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_control = buf.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = buf.len();
        hdr
    }

    #[cfg(target_os = "linux")]
    fn timestamp_from_test_cmsg(
        cmsg_type: libc::c_int,
        tss: &[libc::timespec; 3],
    ) -> Option<RxTimestamp> {
        let mut buf = TimestampCmsgBuffer::new();
        let mut hdr = unsafe { empty_msghdr_for_cmsg(&mut buf) };
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = cmsg_type;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of::<[libc::timespec; 3]>() as libc::c_uint) as usize;
            std::ptr::copy_nonoverlapping(
                tss.as_ptr(),
                libc::CMSG_DATA(cmsg) as *mut libc::timespec,
                3,
            );
            hdr.msg_controllen = (*cmsg).cmsg_len;
            timestamp_from_cmsgs(&hdr)
        }
    }

    #[cfg(target_os = "linux")]
    fn timestampns_from_test_cmsg(ts: libc::timespec) -> Option<RxTimestamp> {
        let mut buf = TimestampCmsgBuffer::new();
        let mut hdr = unsafe { empty_msghdr_for_cmsg(&mut buf) };
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_TIMESTAMPNS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of::<libc::timespec>() as libc::c_uint) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut libc::timespec, ts);
            hdr.msg_controllen = (*cmsg).cmsg_len;
            timestamp_from_cmsgs(&hdr)
        }
    }
}
