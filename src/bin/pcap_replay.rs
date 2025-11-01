use socket2::{Domain, Protocol, Socket, Type};
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("usage: pcap_replay <pcap_file> <group> <port> <iface_ipv4> <pps> [report_ms]");
        std::process::exit(2);
    }
    let path = &args[1];
    let group: Ipv4Addr = args[2].parse()?;
    let port: u16 = args[3].parse()?;
    let iface: Ipv4Addr = args[4].parse()?;
    let pps: u64 = args[5].parse()?;
    let nanos_per_pkt = if pps == 0 { 0 } else { 1_000_000_000u64 / pps };
    let report_ms: u64 = if args.len() > 6 { args[6].parse().unwrap_or(1000) } else { 1000 };

    // Open destination socket
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.bind(&SocketAddr::new(IpAddr::V4(iface), 0).into())?;
    sock.set_multicast_loop_v4(false)?;
    sock.set_multicast_ttl_v4(1)?;
    let dest = SocketAddr::new(IpAddr::V4(group), port);

    // Read pcap
    let mut f = File::open(path)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    let mut off: usize;
    if data.len() < 24 { anyhow::bail!("pcap too small"); }
    let magic = u32::from_le_bytes([data[0],data[1],data[2],data[3]]);
    let le = magic == 0xA1B2C3D4 || magic == 0xA1B23C4D; // basic check
    off = 24; // skip global header
    let start = std::time::Instant::now();
    let mut last_report = start;
    let mut sent_last = 0u64;
    let mut sent = 0u64;
    while off + 16 <= data.len() {
        let (incl_len, _) = if le { read_le_u32(&data, off + 8) } else { read_be_u32(&data, off + 8) };
        off += 16;
        if off + (incl_len as usize) > data.len() { break; }
        let pkt = &data[off..off + incl_len as usize];
        off += incl_len as usize;
        // Best-effort: assume the captured payload is the UDP payload (not full frame)
        let _ = sock.send_to(pkt, &dest.into());
        sent += 1;
        if nanos_per_pkt > 0 { busy_sleep_nanos(nanos_per_pkt); }
        if last_report.elapsed().as_millis() as u64 >= report_ms {
            let interval = last_report.elapsed().as_secs_f64();
            let delta = sent - sent_last;
            let mpps = (delta as f64) / 1_000_000.0 / interval;
            eprintln!("rate: {:.3} Mpps ({} pkts in {:.3} s)", mpps, delta, interval);
            last_report = std::time::Instant::now();
            sent_last = sent;
        }
    }
    eprintln!("replayed {} packets in {:?}", sent, start.elapsed());
    Ok(())
}

#[inline] fn read_le_u32(b: &[u8], off: usize) -> (u32, usize) { (u32::from_le_bytes([b[off],b[off+1],b[off+2],b[off+3]]), 4) }
#[inline] fn read_be_u32(b: &[u8], off: usize) -> (u32, usize) { (u32::from_be_bytes([b[off],b[off+1],b[off+2],b[off+3]]), 4) }

#[inline]
fn busy_sleep_nanos(ns: u64) {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_nanos() as u64 >= ns { break; }
        std::hint::spin_loop();
    }
}


