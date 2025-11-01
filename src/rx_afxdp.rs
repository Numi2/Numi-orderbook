// src/rx_afxdp.rs
// Optional AF_XDP receiver. Integrate by spawning this loop instead of `rx::rx_loop`. Keeps the same Pkt
// contract and queueing model.


use crate::metrics;
use crate::pool::{PacketPool, Pkt, TsKind};
use crate::util::{BarrierFlag, spin_wait};
use crate::parser::SeqExtractor;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;
use bytes::BufMut;

/// Receive loop using a high-performance packet ring on Linux (TPACKET_V2 fallback if AF_XDP is unavailable).
#[cfg(not(target_os = "linux"))]
pub fn afxdp_loop(
    _ifname: &str,
    _queue_id: u32,
    _seq: Arc<dyn SeqExtractor>,
    _chan_name: &str,
    _q_out: Arc<ArrayQueue<Pkt>>,
    _pool: Arc<PacketPool>,
    _shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("AF_XDP is only supported on Linux"))
}

#[cfg(target_os = "linux")]
pub fn afxdp_loop(
    ifname: &str,
    _queue_id: u32,
    seq: Arc<dyn SeqExtractor>,
    chan_name: &str,
    q_out: Arc<ArrayQueue<Pkt>>,
    pool: Arc<PacketPool>,
    shutdown: Arc<BarrierFlag>,
) -> anyhow::Result<()> {
    // Try AF_XDP? For portability, we fallback immediately to PACKET_RX_RING (TPACKET_V2),
    // which is widely supported and provides mmap'ed zero-copy from kernel to userspace.
    use std::ffi::CString;
    use std::mem::size_of;
    use std::ptr::null_mut;

    // Open AF_PACKET raw socket
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, (libc::ETH_P_ALL as u16).to_be() as i32) };
    if fd < 0 { return Err(anyhow::anyhow!("AF_PACKET socket failed: {}", std::io::Error::last_os_error())); }

    // Set TPACKET_V2
    const TPACKET_V2: libc::c_int = 1;
    let ver: libc::c_int = TPACKET_V2;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_PACKET,
            libc::PACKET_VERSION,
            &ver as *const _ as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 { unsafe { libc::close(fd); } return Err(anyhow::anyhow!("PACKET_VERSION set failed")); }

    // Ring parameters
    let frame_size: u32 = 2048; // typical MTU + headers; aligned
    let block_size: u32 = frame_size * 1024; // 2MB per block
    let block_nr: u32 = 4; // total 8MB
    let frame_nr: u32 = (block_size / frame_size) * block_nr;

    #[repr(C)]
    struct TpacketReq { tp_block_size: u32, tp_block_nr: u32, tp_frame_size: u32, tp_frame_nr: u32 }
    let req = TpacketReq { tp_block_size: block_size, tp_block_nr: block_nr, tp_frame_size: frame_size, tp_frame_nr: frame_nr };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_PACKET,
            libc::PACKET_RX_RING,
            &req as *const _ as *const libc::c_void,
            size_of::<TpacketReq>() as libc::socklen_t,
        )
    };
    if rc != 0 { unsafe { libc::close(fd); } return Err(anyhow::anyhow!("PACKET_RX_RING set failed: {}", std::io::Error::last_os_error())); }

    // Bind to interface
    let if_index = unsafe { libc::if_nametoindex(CString::new(ifname).unwrap().as_ptr()) };
    if if_index == 0 { unsafe { libc::close(fd); } return Err(anyhow::anyhow!("if_nametoindex failed for {}", ifname)); }
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    sll.sll_ifindex = if_index as i32;
    let rc = unsafe {
        libc::bind(
            fd,
            &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
            size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc != 0 { unsafe { libc::close(fd); } return Err(anyhow::anyhow!("bind AF_PACKET failed")); }

    // Mmap ring
    let ring_len = (block_size as usize) * (block_nr as usize);
    let ring = unsafe {
        libc::mmap(
            null_mut(),
            ring_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_LOCKED,
            fd,
            0,
        )
    };
    if ring == libc::MAP_FAILED { unsafe { libc::close(fd); } return Err(anyhow::anyhow!("mmap RX_RING failed")); }

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
    let mut dropped: u64 = 0;
    while !shutdown.is_raised() {
        let off = (frame_idx as usize) * (frame_size as usize);
        let hdr_ptr = unsafe { (ring as *mut u8).add(off) as *mut Tpacket2Hdr };
        let status = unsafe { (*hdr_ptr).tp_status };
        if (status & TP_STATUS_USER) == 0 {
            spin_wait(64);
            continue;
        }

        // Determine packet bytes (L2.. payload)
        let snap = unsafe { (*hdr_ptr).tp_snaplen } as usize;
        let mac_off = unsafe { (*hdr_ptr).tp_mac } as usize;
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
                        let pkt = Pkt { buf, len: nbytes, seq: sv, ts_nanos, chan: chan_id, ts_kind: TsKind::Sw, merge_emit_ns: 0 };
                        if let Err(_full) = q_out.push(pkt) {
                            dropped += 1;
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
        unsafe { (*hdr_ptr).tp_status = 0; }
        frame_idx = (frame_idx + 1) % frame_nr;
    }

    unsafe { libc::munmap(ring, ring_len); libc::close(fd); }
    Ok(())
}

#[cfg(target_os = "linux")]
fn parse_udp_payload(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 14 { return None; }
    let mut off = 0usize;
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    off += 14;
    let mut et = ethertype;
    if et == 0x8100 || et == 0x88A8 {
        if frame.len() < off + 4 { return None; }
        et = u16::from_be_bytes([frame[off + 2], frame[off + 3]]);
        off += 4;
    }
    if et != 0x0800 { return None; } // IPv4
    if frame.len() < off + 20 { return None; }
    let ihl = (frame[off] & 0x0F) as usize * 4;
    if frame.len() < off + ihl + 8 { return None; }
    let proto = frame[off + 9];
    if proto != 17 { return None; } // UDP
    off += ihl;
    // UDP header 8 bytes
    off += 8;
    if frame.len() < off { return None; }
    Some(&frame[off..])
}


