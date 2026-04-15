// src/rx_packet_mmap.rs
// PACKET_MMAP receiver. This is a high-throughput AF_PACKET / PACKET_RX_RING
// fallback that keeps the same Pkt contract and queueing model as `rx::rx_loop`.
//
// This is intentionally not labeled as AF_XDP: a real AF_XDP/XSK path must own
// UMEM fill/completion rings and avoid the payload copy below.

#[cfg(target_os = "linux")]
use crate::metrics;
use crate::parser::SeqExtractor;
#[cfg(target_os = "linux")]
use crate::pool::PktBuf;
#[cfg(target_os = "linux")]
use crate::pool::TsKind;
use crate::pool::{PacketPool, Pkt};
use crate::spsc::SpscQueue;
use crate::util::BarrierFlag;
#[cfg(target_os = "linux")]
use bytes::BufMut;
use std::sync::Arc;

/// Receive loop using PACKET_RX_RING on Linux.
#[cfg(not(target_os = "linux"))]
pub fn packet_mmap_loop(
    _ifname: &str,
    _opts: PacketMmapOptions,
    _seq: Arc<dyn SeqExtractor>,
    _chan_name: &str,
    _q_out: Arc<SpscQueue<Pkt>>,
    _pool: Arc<PacketPool>,
    _shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("PACKET_MMAP is only supported on Linux"))
}

#[derive(Debug, Clone, Copy)]
pub struct PacketMmapOptions {
    pub queue_id: u32,
    pub frame_size: u32,
    pub frames_per_block: u32,
    pub block_count: u32,
}

impl PacketMmapOptions {
    pub fn validate(self) -> anyhow::Result<()> {
        if self.frame_size < 2048 || !self.frame_size.is_power_of_two() {
            anyhow::bail!("packet_mmap.frame_size must be a power of two and at least 2048");
        }
        if self.frames_per_block == 0 {
            anyhow::bail!("packet_mmap.frames_per_block must be > 0");
        }
        if self.block_count == 0 {
            anyhow::bail!("packet_mmap.block_count must be > 0");
        }
        let block_size = self
            .frame_size
            .checked_mul(self.frames_per_block)
            .ok_or_else(|| anyhow::anyhow!("packet_mmap block size overflow"))?;
        let frame_count = self
            .frames_per_block
            .checked_mul(self.block_count)
            .ok_or_else(|| anyhow::anyhow!("packet_mmap frame count overflow"))?;
        if block_size == 0 || frame_count == 0 {
            anyhow::bail!("packet_mmap ring geometry must be non-empty");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn validate_for_linux(self) -> anyhow::Result<()> {
        self.validate()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            anyhow::bail!("could not read system page size");
        }
        let block_size = self.frame_size as usize * self.frames_per_block as usize;
        if block_size % page_size as usize != 0 {
            anyhow::bail!(
                "packet_mmap block size must be a multiple of page size: block_size={} page_size={}",
                block_size,
                page_size
            );
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn block_size(self) -> u32 {
        self.frame_size * self.frames_per_block
    }

    #[cfg(target_os = "linux")]
    fn frame_count(self) -> u32 {
        self.frames_per_block * self.block_count
    }
}

#[cfg(target_os = "linux")]
pub fn packet_mmap_loop(
    ifname: &str,
    opts: PacketMmapOptions,
    seq: Arc<dyn SeqExtractor>,
    chan_name: &str,
    q_out: Arc<SpscQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    opts.validate_for_linux()?;
    // PACKET_RX_RING (TPACKET_V2) provides mmap'ed delivery from kernel to userspace,
    // followed by a copy into the pooled packet buffer used by the rest of the pipeline.
    use std::ffi::CString;
    use std::mem::size_of;
    use std::ptr::null_mut;

    // Open AF_PACKET raw socket (fallback path)
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW,
            (libc::ETH_P_ALL as u16).to_be() as i32,
        )
    };
    if raw_fd < 0 {
        return Err(anyhow::anyhow!(
            "AF_PACKET socket failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let fd = PacketFd(raw_fd);

    // Set TPACKET_V2
    const TPACKET_V2: libc::c_int = 1;
    let ver: libc::c_int = TPACKET_V2;
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw(),
            libc::SOL_PACKET,
            libc::PACKET_VERSION,
            &ver as *const _ as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "PACKET_VERSION set failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Ring parameters
    let frame_size = opts.frame_size;
    let block_size = opts.block_size();
    let block_nr = opts.block_count;
    let frame_nr = opts.frame_count();

    #[repr(C)]
    struct TpacketReq {
        tp_block_size: u32,
        tp_block_nr: u32,
        tp_frame_size: u32,
        tp_frame_nr: u32,
    }
    let req = TpacketReq {
        tp_block_size: block_size,
        tp_block_nr: block_nr,
        tp_frame_size: frame_size,
        tp_frame_nr: frame_nr,
    };
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw(),
            libc::SOL_PACKET,
            libc::PACKET_RX_RING,
            &req as *const _ as *const libc::c_void,
            size_of::<TpacketReq>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "PACKET_RX_RING set failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Bind to interface
    let ifname_c = CString::new(ifname)
        .map_err(|_| anyhow::anyhow!("interface name contains interior NUL: {ifname:?}"))?;
    let if_index = unsafe { libc::if_nametoindex(ifname_c.as_ptr()) };
    if if_index == 0 {
        return Err(anyhow::anyhow!(
            "if_nametoindex failed for {}: {}",
            ifname,
            std::io::Error::last_os_error()
        ));
    }
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    sll.sll_ifindex = if_index as i32;
    let rc = unsafe {
        libc::bind(
            fd.as_raw(),
            &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
            size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "bind AF_PACKET failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Enable PACKET_FANOUT to distribute frames across multiple sockets/threads
    // when spawning multiple workers. Use HASH policy for even distribution.
    {
        const PACKET_FANOUT: libc::c_int = 18; // from linux/if_packet.h
        const PACKET_FANOUT_HASH: u16 = 0;
        let group_id = fanout_group_id(ifname, chan_name);
        let val: u32 = ((group_id as u32) << 16) | (PACKET_FANOUT_HASH as u32);
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw(),
                libc::SOL_PACKET,
                PACKET_FANOUT,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<u32>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(anyhow::anyhow!(
                "PACKET_FANOUT set failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    // Mmap ring
    let ring_len = (block_size as usize) * (block_nr as usize);
    let ring = unsafe {
        libc::mmap(
            null_mut(),
            ring_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_LOCKED,
            fd.as_raw(),
            0,
        )
    };
    if ring == libc::MAP_FAILED {
        return Err(anyhow::anyhow!(
            "mmap RX_RING failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ring = MmapRing::new(ring, ring_len);

    // Structures for TPACKET_V2 frames
    #[repr(C)]
    struct Tpacket2Hdr {
        tp_status: u32,
        tp_len: u32,
        tp_snaplen: u32,
        tp_mac: u16,
        tp_net: u16,
        tp_sec: u32,
        tp_nsec: u32,
        tp_vlan_tci: u16,
        tp_vlan_tpid: u16,
        // followed by padding
    }

    const TP_STATUS_USER: u32 = 1u32; // bit 0

    let chan_id = if chan_name == "A" { b'A' } else { b'B' };
    let mut frame_idx: u32 = 0;
    while !shutdown.is_raised() {
        let off = (frame_idx as usize) * (frame_size as usize);
        let hdr_ptr = unsafe { ring.ptr().add(off) as *mut Tpacket2Hdr };
        let status = unsafe { (*hdr_ptr).tp_status };
        if (status & TP_STATUS_USER) == 0 {
            crate::util::spin_wait(64);
            continue;
        }

        // Determine packet bytes (L2.. payload)
        let snap = unsafe { (*hdr_ptr).tp_snaplen } as usize;
        let mac_off = unsafe { (*hdr_ptr).tp_mac } as usize;
        if mac_off > frame_size as usize || snap > (frame_size as usize).saturating_sub(mac_off) {
            metrics::inc_rx_drop(chan_name);
            unsafe {
                (*hdr_ptr).tp_status = 0;
            }
            frame_idx = (frame_idx + 1) % frame_nr;
            continue;
        }
        let data_ptr = unsafe { (hdr_ptr as *mut u8).add(mac_off) };
        let frame = unsafe { std::slice::from_raw_parts(data_ptr, snap) };

        // Parse UDP payload offset (Ethernet + IPv4 + UDP), handle optional single VLAN
        if let Some(udp_payload) = parse_udp_payload(frame) {
            let nbytes = udp_payload.len();
            // Use kernel-provided timestamp from TPACKET_V2 header
            let ts_nanos = (unsafe { (*hdr_ptr).tp_sec } as u64) * 1_000_000_000u64
                + (unsafe { (*hdr_ptr).tp_nsec } as u64);
            let mut buf = pool.get();
            unsafe {
                let dst = {
                    let s = buf.chunk_mut();
                    std::slice::from_raw_parts_mut(s.as_mut_ptr() as *mut u8, s.len())
                };
                if nbytes <= dst.len() {
                    std::ptr::copy_nonoverlapping(udp_payload.as_ptr(), dst.as_mut_ptr(), nbytes);
                    buf.advance_mut(nbytes);
                    let seqv = seq.extract_seq(&buf);
                    if let Some(sv) = seqv {
                        let pkt = Pkt {
                            buf: PktBuf::Bytes(buf),
                            len: nbytes,
                            seq: sv,
                            ts_nanos,
                            chan: chan_id,
                            _ts_kind: TsKind::Sw,
                            merge_emit_ns: 0,
                        };
                        if let Err(_full) = q_out.push(pkt) {
                            metrics::inc_rx_drop(chan_name);
                        } else {
                            metrics::inc_rx(chan_name, nbytes);
                        }
                    } else {
                        pool.put(buf);
                    }
                } else {
                    pool.put(buf);
                }
            }
        }

        // Release frame back to kernel
        unsafe {
            (*hdr_ptr).tp_status = 0;
        }
        frame_idx = (frame_idx + 1) % frame_nr;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
struct PacketFd(libc::c_int);

#[cfg(target_os = "linux")]
impl PacketFd {
    fn as_raw(&self) -> libc::c_int {
        self.0
    }
}

#[cfg(target_os = "linux")]
impl Drop for PacketFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct MmapRing {
    ptr: *mut u8,
    len: usize,
}

#[cfg(target_os = "linux")]
impl MmapRing {
    fn new(ptr: *mut libc::c_void, len: usize) -> Self {
        Self {
            ptr: ptr.cast::<u8>(),
            len,
        }
    }

    fn ptr(&self) -> *mut u8 {
        self.ptr
    }
}

#[cfg(target_os = "linux")]
impl Drop for MmapRing {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                libc::munmap(self.ptr.cast::<libc::c_void>(), self.len);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn fanout_group_id(ifname: &str, chan_name: &str) -> u16 {
    let mut hash = 0x811c9dc5u32;
    for b in ifname.bytes().chain(chan_name.bytes()) {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x01000193);
    }
    let group = (hash & 0xffff) as u16;
    if group == 0 {
        1
    } else {
        group
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn fanout_group_is_stable_per_interface_and_channel() {
        assert_eq!(fanout_group_id("eth0", "A"), fanout_group_id("eth0", "A"));
        assert_ne!(fanout_group_id("eth0", "A"), 0);
        assert_ne!(fanout_group_id("eth0", "A"), fanout_group_id("eth0", "B"));
    }

    #[test]
    fn parses_ipv4_udp_payload() {
        let mut frame = vec![0u8; 14 + 20 + 8 + 4];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = 14;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&32u16.to_be_bytes());
        frame[ip + 9] = 17;
        let udp = ip + 20;
        frame[udp + 4..udp + 6].copy_from_slice(&12u16.to_be_bytes());
        frame[udp + 8..udp + 12].copy_from_slice(b"ABCD");

        assert_eq!(parse_udp_payload(&frame), Some(&b"ABCD"[..]));
    }

    #[test]
    fn parses_single_vlan_ipv4_udp_payload() {
        let mut frame = vec![0u8; 14 + 4 + 20 + 8 + 3];
        frame[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = 18;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&31u16.to_be_bytes());
        frame[ip + 9] = 17;
        let udp = ip + 20;
        frame[udp + 4..udp + 6].copy_from_slice(&11u16.to_be_bytes());
        frame[udp + 8..udp + 11].copy_from_slice(b"XYZ");

        assert_eq!(parse_udp_payload(&frame), Some(&b"XYZ"[..]));
    }

    #[test]
    fn rejects_non_udp_payload() {
        let mut frame = vec![0u8; 14 + 20 + 8];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&28u16.to_be_bytes());
        frame[14 + 9] = 6;
        assert!(parse_udp_payload(&frame).is_none());
    }

    #[test]
    fn rejects_truncated_udp_length() {
        let mut frame = vec![0u8; 14 + 20 + 8 + 2];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = 14;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&30u16.to_be_bytes());
        frame[ip + 9] = 17;
        let udp = ip + 20;
        frame[udp + 4..udp + 6].copy_from_slice(&12u16.to_be_bytes());
        assert!(parse_udp_payload(&frame).is_none());
    }

    #[test]
    fn rejects_fragmented_ipv4_udp_payload() {
        let mut frame = vec![0u8; 14 + 20 + 8];
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = 14;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&28u16.to_be_bytes());
        frame[ip + 6..ip + 8].copy_from_slice(&0x2000u16.to_be_bytes());
        frame[ip + 9] = 17;
        let udp = ip + 20;
        frame[udp + 4..udp + 6].copy_from_slice(&8u16.to_be_bytes());
        assert!(parse_udp_payload(&frame).is_none());
    }

    #[test]
    fn packet_mmap_options_reject_bad_geometry() {
        assert!(PacketMmapOptions {
            queue_id: 0,
            frame_size: 1500,
            frames_per_block: 1024,
            block_count: 4,
        }
        .validate()
        .is_err());
        assert!(PacketMmapOptions {
            queue_id: 0,
            frame_size: 2048,
            frames_per_block: 0,
            block_count: 4,
        }
        .validate()
        .is_err());
    }
}

#[cfg(target_os = "linux")]
fn parse_udp_payload(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 14 {
        return None;
    }
    let mut off = 0usize;
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    off += 14;
    let mut et = ethertype;
    if et == 0x8100 || et == 0x88A8 {
        if frame.len() < off + 4 {
            return None;
        }
        et = u16::from_be_bytes([frame[off + 2], frame[off + 3]]);
        off += 4;
    }
    if et != 0x0800 {
        return None;
    } // IPv4
    if frame.len() < off + 20 {
        return None;
    }
    let ip_start = off;
    let ihl = (frame[ip_start] & 0x0F) as usize * 4;
    if ihl < 20 || frame.len() < ip_start + ihl + 8 {
        return None;
    }
    let total_len = u16::from_be_bytes([frame[ip_start + 2], frame[ip_start + 3]]) as usize;
    if total_len < ihl + 8 || frame.len() < ip_start + total_len {
        return None;
    }
    let fragment = u16::from_be_bytes([frame[ip_start + 6], frame[ip_start + 7]]);
    if (fragment & 0x3fff) != 0 {
        return None;
    }
    let proto = frame[ip_start + 9];
    if proto != 17 {
        return None;
    } // UDP
    let udp_start = ip_start + ihl;
    let udp_len = u16::from_be_bytes([frame[udp_start + 4], frame[udp_start + 5]]) as usize;
    if udp_len < 8 || udp_len > total_len - ihl {
        return None;
    }
    let payload_start = udp_start + 8;
    let payload_end = udp_start + udp_len;
    Some(&frame[payload_start..payload_end])
}
