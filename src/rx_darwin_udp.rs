// src/rx_darwin_udp.rs
// macOS UDP receive path for local performance work. Darwin has no AF_XDP and
// no public recvmmsg(2), so this path uses the real recvmsg_x batch syscall when
// available and falls back to recvmsg(2) without changing packet semantics.

use crate::config::TimestampingMode;
use crate::metrics;
use crate::parser::SeqExtractor;
use crate::pool::{PacketPool, Pkt, PktBuf, TsKind};
use crate::rx::{flush_rx_packet_batch, stage_or_flush_rx_packet};
use crate::spsc::{AdaptiveBatchCap, SpscQueue, DEFAULT_BATCH_CAP};
use crate::util::{mach_absolute_to_nanos, now_nanos};
use anyhow::Context;
use bytes::{BufMut, BytesMut};
use log::{debug, warn};
use nix::errno::Errno;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::sync::Arc;

pub struct DarwinUdpRxConfig {
    pub spin_loops_per_yield: u32,
    pub rx_batch: usize,
    pub ts_mode: Option<TimestampingMode>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DarwinMsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: usize,
}

impl DarwinMsghdrX {
    fn empty() -> Self {
        Self {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: std::ptr::null_mut(),
            msg_iovlen: 0,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
            msg_datalen: 0,
        }
    }
}

pub fn rx_darwin_udp_loop(
    chan_name: &str,
    sock: &UdpSocket,
    seq: Arc<dyn SeqExtractor>,
    q_out: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<crate::util::BarrierFlag>,
    cfg: DarwinUdpRxConfig,
) -> anyhow::Result<()> {
    let DarwinUdpRxConfig {
        spin_loops_per_yield,
        rx_batch,
        ts_mode,
    } = cfg;
    let fd = sock.as_raw_fd();
    let mut dropped: u64 = 0;
    let chan_id = if chan_name == "A" { b'A' } else { b'B' };

    sock.set_nonblocking(true).context("set nonblocking")?;

    let wants_timestamps = match ts_mode.as_ref().unwrap_or(&TimestampingMode::Off) {
        TimestampingMode::Off => false,
        TimestampingMode::Software => true,
        mode @ (TimestampingMode::Hardware | TimestampingMode::HardwareRaw) => {
            anyhow::bail!(
                "timestamping={:?} is configured but Darwin UDP RX only supports software timestamps",
                mode
            );
        }
    };

    let batch = rx_batch.clamp(1, DARWIN_RECVMSG_X_BATCH_LIMIT);
    let mut recvmsg_x_enabled = batch > 1;

    let mut bufs: Vec<BytesMut> = if recvmsg_x_enabled {
        (0..batch).map(|_| BytesMut::new()).collect()
    } else {
        Vec::new()
    };
    let mut iovecs: Vec<libc::iovec> = if recvmsg_x_enabled {
        (0..batch)
            .map(|_| libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut hdrs: Vec<DarwinMsghdrX> = if recvmsg_x_enabled {
        (0..batch).map(|_| DarwinMsghdrX::empty()).collect()
    } else {
        Vec::new()
    };
    let mut cmsgs: Vec<TimestampCmsgBuffer> = if recvmsg_x_enabled && wants_timestamps {
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
        let mut loop_now_cache: Option<u64> = None;
        if !wants_timestamps {
            loop_now_cache = Some(now_nanos());
        }

        if recvmsg_x_enabled {
            let batch_progress = unsafe {
                recvmsg_x_batch(
                    fd,
                    batch,
                    wants_timestamps,
                    &pool,
                    &seq,
                    chan_name,
                    chan_id,
                    &q_out,
                    &mut bufs,
                    &mut iovecs,
                    &mut hdrs,
                    &mut cmsgs,
                    &mut pending_pkts,
                    &mut ring_batch_cap,
                    &mut dropped,
                    &mut loop_now_cache,
                )
            };
            match batch_progress {
                Ok(BatchProgress::Progress) => {
                    progressed = true;
                }
                Ok(BatchProgress::NoProgress) => {}
                Ok(BatchProgress::Unsupported) => {
                    recvmsg_x_enabled = false;
                    bufs.clear();
                    iovecs.clear();
                    hdrs.clear();
                    cmsgs.clear();
                    warn!(
                        "{chan_name}_rx: Darwin recvmsg_x unavailable for this socket; falling back to recvmsg"
                    );
                }
                Err(err) => {
                    flush_rx_packet_batch(
                        chan_name,
                        &q_out,
                        &pool,
                        &mut pending_pkts,
                        &mut ring_batch_cap,
                        &mut dropped,
                    );
                    return Err(err);
                }
            }
        } else {
            for _ in 0..batch {
                if shutdown.is_raised() {
                    break;
                }
                match recv_one_packet(
                    fd,
                    wants_timestamps,
                    &pool,
                    &seq,
                    chan_name,
                    chan_id,
                    &q_out,
                    &mut pending_pkts,
                    &mut ring_batch_cap,
                    &mut dropped,
                    &mut loop_now_cache,
                )? {
                    PacketProgress::Progress => progressed = true,
                    PacketProgress::NoProgress => break,
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

enum BatchProgress {
    Progress,
    NoProgress,
    Unsupported,
}

enum PacketProgress {
    Progress,
    NoProgress,
}

const DARWIN_RECVMSG_X_BATCH_LIMIT: usize = 256;
const SYS_RECVMSG_X: libc::c_int = 480;
const SCM_TIMESTAMP_MONOTONIC: libc::c_int = 0x04;

#[allow(clippy::too_many_arguments)]
unsafe fn recvmsg_x_batch(
    fd: libc::c_int,
    batch: usize,
    wants_timestamps: bool,
    pool: &PacketPool,
    seq: &Arc<dyn SeqExtractor>,
    chan_name: &str,
    chan_id: u8,
    q_out: &SpscQueue<Pkt>,
    bufs: &mut [BytesMut],
    iovecs: &mut [libc::iovec],
    hdrs: &mut [DarwinMsghdrX],
    cmsgs: &mut [TimestampCmsgBuffer],
    pending_pkts: &mut Vec<Pkt>,
    ring_batch_cap: &mut AdaptiveBatchCap,
    dropped: &mut u64,
    loop_now_cache: &mut Option<u64>,
) -> anyhow::Result<BatchProgress> {
    for i in 0..batch {
        bufs[i] = pool.get();
        let s = bufs[i].chunk_mut();
        iovecs[i].iov_base = s.as_mut_ptr() as *mut libc::c_void;
        iovecs[i].iov_len = s.len();

        hdrs[i] = DarwinMsghdrX::empty();
        hdrs[i].msg_iov = &mut iovecs[i] as *mut libc::iovec;
        hdrs[i].msg_iovlen = 1;
        if wants_timestamps {
            cmsgs[i].clear();
            hdrs[i].msg_control = cmsgs[i].as_mut_ptr() as *mut libc::c_void;
            hdrs[i].msg_controllen = cmsgs[i].len() as libc::socklen_t;
        }
    }

    let ret = libc::syscall(
        SYS_RECVMSG_X,
        fd,
        hdrs.as_mut_ptr(),
        batch as libc::c_uint,
        libc::MSG_DONTWAIT,
    );

    if ret < 0 {
        let err = Errno::last();
        recycle_prepared_buffers(bufs, pool);
        if err == Errno::EAGAIN || err == Errno::EWOULDBLOCK || err == Errno::EINTR {
            return Ok(BatchProgress::NoProgress);
        }
        if is_recvmsg_x_unsupported(err) {
            return Ok(BatchProgress::Unsupported);
        }
        return Err(anyhow::anyhow!(
            "recvmsg_x error: {}",
            std::io::Error::from(err)
        ));
    }

    if ret == 0 {
        recycle_prepared_buffers(bufs, pool);
        return Ok(BatchProgress::NoProgress);
    }

    let count = (ret as usize).min(batch);
    for i in 0..count {
        let mut buf = std::mem::take(&mut bufs[i]);
        let n = hdrs[i].msg_datalen;
        if (hdrs[i].msg_flags & libc::MSG_TRUNC) != 0 || n > iovecs[i].iov_len {
            pool.put(buf);
            record_rx_drops(chan_name, 1, dropped, "truncated datagram");
            continue;
        }
        buf.advance_mut(n);
        if let Some(sv) = seq.extract_seq(&buf) {
            let rx_ts =
                packet_timestamp_from_msg_x(wants_timestamps.then_some(&hdrs[i]), loop_now_cache);
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
                q_out,
                pool,
                pkt,
                pending_pkts,
                ring_batch_cap,
                dropped,
            );
        } else {
            pool.put(buf);
        }
    }
    for buf in bufs.iter_mut().take(batch).skip(count) {
        let b = std::mem::take(buf);
        if b.capacity() > 0 {
            pool.put(b);
        }
    }

    Ok(BatchProgress::Progress)
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn recv_one_packet(
    fd: libc::c_int,
    wants_timestamps: bool,
    pool: &PacketPool,
    seq: &Arc<dyn SeqExtractor>,
    chan_name: &str,
    chan_id: u8,
    q_out: &SpscQueue<Pkt>,
    pending_pkts: &mut Vec<Pkt>,
    ring_batch_cap: &mut AdaptiveBatchCap,
    dropped: &mut u64,
    loop_now_cache: &mut Option<u64>,
) -> anyhow::Result<PacketProgress> {
    let mut buf = pool.get();
    let dst = unsafe {
        let s = buf.chunk_mut();
        std::slice::from_raw_parts_mut(s.as_mut_ptr(), s.len())
    };

    let res = if wants_timestamps {
        recvmsg_one(fd, dst, loop_now_cache)
    } else {
        unsafe {
            let n = libc::recv(
                fd,
                dst.as_mut_ptr() as *mut libc::c_void,
                dst.len(),
                libc::MSG_DONTWAIT,
            );
            if n >= 0 {
                Ok(Some((n as usize, loop_now_cache.unwrap(), TsKind::Sw)))
            } else {
                Err(Errno::last())
            }
        }
    };

    match res {
        Ok(Some((n, ts, kind))) => {
            unsafe {
                buf.advance_mut(n);
            }
            if let Some(sv) = seq.extract_seq(&buf) {
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
                    q_out,
                    pool,
                    pkt,
                    pending_pkts,
                    ring_batch_cap,
                    dropped,
                );
            } else {
                pool.put(buf);
            }
            Ok(PacketProgress::Progress)
        }
        Ok(None) => {
            pool.put(buf);
            record_rx_drops(chan_name, 1, dropped, "truncated datagram");
            Ok(PacketProgress::Progress)
        }
        Err(err) => {
            pool.put(buf);
            if err == Errno::EAGAIN || err == Errno::EWOULDBLOCK || err == Errno::EINTR {
                Ok(PacketProgress::NoProgress)
            } else {
                Err(anyhow::anyhow!("recv error: {}", std::io::Error::from(err)))
            }
        }
    }
}

#[inline]
fn is_recvmsg_x_unsupported(err: Errno) -> bool {
    err == Errno::ENOSYS || err == Errno::EOPNOTSUPP || err == Errno::ENOTSUP
}

fn recycle_prepared_buffers(bufs: &mut [BytesMut], pool: &PacketPool) {
    for buf in bufs {
        let b = std::mem::take(buf);
        if b.capacity() > 0 {
            pool.put(b);
        }
    }
}

#[inline]
fn record_rx_drops(chan_name: &str, count: usize, dropped: &mut u64, reason: &str) {
    if count == 0 {
        return;
    }
    let before = *dropped;
    *dropped = dropped.saturating_add(count as u64);
    metrics::inc_rx_drop_batch(chan_name, count);
    if before == 0 || before / 10_000 != *dropped / 10_000 {
        debug!("{}_rx: {}, dropped={}", chan_name, reason, *dropped);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RxTimestamp {
    nanos: u64,
    kind: TsKind,
}

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

#[inline]
fn packet_timestamp_from_msg_x(
    hdr: Option<&DarwinMsghdrX>,
    cache: &mut Option<u64>,
) -> RxTimestamp {
    if let Some(hdr) = hdr {
        if let Some(ts) = unsafe { timestamp_from_msg_x_cmsgs(hdr) } {
            return ts;
        }
    }
    RxTimestamp {
        nanos: cached_now(cache),
        kind: TsKind::Sw,
    }
}

#[inline]
fn packet_timestamp_from_msg(hdr: Option<&libc::msghdr>, cache: &mut Option<u64>) -> RxTimestamp {
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

const TIMESTAMP_CMSG_SPACE: usize =
    unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::timeval>() as libc::c_uint) as usize };

#[repr(C, align(16))]
struct TimestampCmsgBuffer {
    bytes: [u8; TIMESTAMP_CMSG_SPACE],
}

impl TimestampCmsgBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; TIMESTAMP_CMSG_SPACE],
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.bytes.fill(0);
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

fn recvmsg_one(
    fd: libc::c_int,
    dst: &mut [u8],
    cache: &mut Option<u64>,
) -> Result<Option<(usize, u64, TsKind)>, Errno> {
    let mut iov = libc::iovec {
        iov_base: dst.as_mut_ptr() as *mut libc::c_void,
        iov_len: dst.len(),
    };
    let mut cmsg = TimestampCmsgBuffer::new();
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov as *mut libc::iovec;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = cmsg.len() as libc::socklen_t;

    let n = unsafe { libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(Errno::last());
    }
    if (hdr.msg_flags & libc::MSG_TRUNC) != 0 {
        return Ok(None);
    }

    let ts = packet_timestamp_from_msg(Some(&hdr), cache);
    Ok(Some((n as usize, ts.nanos, ts.kind)))
}

unsafe fn timestamp_from_msg_x_cmsgs(hdr_x: &DarwinMsghdrX) -> Option<RxTimestamp> {
    if hdr_x.msg_control.is_null() || hdr_x.msg_controllen == 0 {
        return None;
    }
    let mut hdr: libc::msghdr = std::mem::zeroed();
    hdr.msg_control = hdr_x.msg_control;
    hdr.msg_controllen = hdr_x.msg_controllen;
    hdr.msg_flags = hdr_x.msg_flags;
    timestamp_from_cmsgs(&hdr)
}

unsafe fn timestamp_from_cmsgs(hdr: &libc::msghdr) -> Option<RxTimestamp> {
    if hdr.msg_controllen == 0 || (hdr.msg_flags & libc::MSG_CTRUNC) != 0 {
        return None;
    }

    let hdr_ptr = hdr as *const libc::msghdr;
    let mut cmsg = libc::CMSG_FIRSTHDR(hdr_ptr);
    while !cmsg.is_null() {
        let cmsg_len = (*cmsg).cmsg_len as usize;
        if cmsg_len < std::mem::size_of::<libc::cmsghdr>() {
            break;
        }
        if (*cmsg).cmsg_level == libc::SOL_SOCKET
            && (*cmsg).cmsg_type == SCM_TIMESTAMP_MONOTONIC
            && cmsg_has_payload(cmsg, std::mem::size_of::<u64>())
        {
            let ticks = (libc::CMSG_DATA(cmsg) as *const u64).read_unaligned();
            if ticks != 0 {
                return Some(RxTimestamp {
                    nanos: mach_absolute_to_nanos(ticks),
                    kind: TsKind::Sw,
                });
            }
        }
        cmsg = libc::CMSG_NXTHDR(hdr_ptr, cmsg);
    }

    None
}

#[inline]
unsafe fn cmsg_has_payload(cmsg: *const libc::cmsghdr, payload_len: usize) -> bool {
    let required = libc::CMSG_LEN(payload_len as libc::c_uint) as usize;
    (*cmsg).cmsg_len as usize >= required
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::mach_absolute_time_ticks;
    use std::time::Duration;

    struct TestSeq;

    impl SeqExtractor for TestSeq {
        fn extract_seq(&self, pkt: &[u8]) -> Option<u64> {
            let bytes: [u8; 8] = pkt.get(0..8)?.try_into().ok()?;
            Some(u64::from_le_bytes(bytes))
        }
    }

    #[test]
    fn monotonic_timestamp_cmsg_converts_mach_ticks_to_nanos() {
        let ticks = mach_absolute_time_ticks();
        let parsed = timestamp_from_test_cmsg(ticks, 0).unwrap();

        assert_eq!(parsed.kind, TsKind::Sw);
        assert_eq!(parsed.nanos, mach_absolute_to_nanos(ticks));
    }

    #[test]
    fn timestamp_cmsg_ignores_control_truncation() {
        let ticks = mach_absolute_time_ticks();
        assert!(timestamp_from_test_cmsg(ticks, libc::MSG_CTRUNC).is_none());
    }

    #[test]
    fn zeroed_control_buffer_does_not_loop_forever() {
        let mut buf = TimestampCmsgBuffer::new();
        let mut hdr = unsafe { empty_msghdr_for_cmsg(&mut buf) };
        hdr.msg_controllen = buf.len() as libc::socklen_t;

        assert!(unsafe { timestamp_from_cmsgs(&hdr) }.is_none());
    }

    #[test]
    #[ignore = "host smoke test for Darwin recvmsg_x UDP delivery"]
    fn recvmsg_x_receives_timestamped_udp_datagram() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let on: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMP_MONOTONIC,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "setsockopt SO_TIMESTAMP_MONOTONIC failed");

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut payload = [0_u8; 16];
        payload[..8].copy_from_slice(&42_u64.to_le_bytes());
        sender
            .send_to(&payload, sock.local_addr().unwrap())
            .unwrap();

        let pool = PacketPool::new(8, 128).unwrap();
        let seq: Arc<dyn SeqExtractor> = Arc::new(TestSeq);
        let q = SpscQueue::new(8);
        let mut bufs = (0..4).map(|_| BytesMut::new()).collect::<Vec<_>>();
        let mut iovecs = (0..4)
            .map(|_| libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            })
            .collect::<Vec<_>>();
        let mut hdrs = (0..4).map(|_| DarwinMsghdrX::empty()).collect::<Vec<_>>();
        let mut cmsgs = (0..4)
            .map(|_| TimestampCmsgBuffer::new())
            .collect::<Vec<_>>();
        let mut pending = Vec::new();
        let mut ring_batch_cap = AdaptiveBatchCap::new(1, 4);
        let mut dropped = 0;
        let mut cache = None;

        for _ in 0..100 {
            match unsafe {
                recvmsg_x_batch(
                    sock.as_raw_fd(),
                    4,
                    true,
                    &pool,
                    &seq,
                    "T",
                    b'T',
                    &q,
                    &mut bufs,
                    &mut iovecs,
                    &mut hdrs,
                    &mut cmsgs,
                    &mut pending,
                    &mut ring_batch_cap,
                    &mut dropped,
                    &mut cache,
                )
            }
            .unwrap()
            {
                BatchProgress::Progress => break,
                BatchProgress::NoProgress => std::thread::sleep(Duration::from_millis(1)),
                BatchProgress::Unsupported => panic!("recvmsg_x unsupported on this host"),
            }
        }

        flush_rx_packet_batch(
            "T",
            &q,
            &pool,
            &mut pending,
            &mut ring_batch_cap,
            &mut dropped,
        );
        let pkt = q.pop().expect("recvmsg_x should deliver one packet");
        assert_eq!(pkt.seq, 42);
        assert_eq!(pkt.len, payload.len());
        assert_eq!(pkt._ts_kind, TsKind::Sw);
        assert!(pkt.ts_nanos > 0);
        pkt.recycle(&pool);
    }

    unsafe fn empty_msghdr_for_cmsg(buf: &mut TimestampCmsgBuffer) -> libc::msghdr {
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_control = buf.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = buf.len() as libc::socklen_t;
        hdr
    }

    fn timestamp_from_test_cmsg(ticks: u64, flags: libc::c_int) -> Option<RxTimestamp> {
        let mut buf = TimestampCmsgBuffer::new();
        let mut hdr = unsafe { empty_msghdr_for_cmsg(&mut buf) };
        hdr.msg_flags = flags;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = SCM_TIMESTAMP_MONOTONIC;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of::<u64>() as libc::c_uint) as libc::socklen_t;
            (libc::CMSG_DATA(cmsg) as *mut u64).write_unaligned(ticks);
            hdr.msg_controllen = (*cmsg).cmsg_len;
            timestamp_from_cmsgs(&hdr)
        }
    }
}
