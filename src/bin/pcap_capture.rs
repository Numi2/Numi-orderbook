use socket2::{Domain, Protocol, Socket, Type};
use std::fs::File;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("usage: pcap_capture <group> <port> <iface_ipv4> <outfile> <seconds>");
        std::process::exit(2);
    }
    let group: Ipv4Addr = args[1].parse()?;
    let port: u16 = args[2].parse()?;
    let iface: Ipv4Addr = args[3].parse()?;
    let out = &args[4];
    let seconds: u64 = args[5].parse()?;

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true).ok();
    #[cfg(any(target_os="linux", target_os="android", target_os="freebsd"))]
    sock.set_reuse_port(true).ok();
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&bind_addr.into())?;
    sock.join_multicast_v4(&group, &iface)?;
    let s: UdpSocket = sock.into();
    s.set_nonblocking(false)?;

    let mut f = File::create(out)?;
    write_pcap_global_header(&mut f)?;
    let start = std::time::Instant::now();
    let mut buf = vec![0u8; 65535];
    loop {
        if start.elapsed().as_secs() >= seconds { break; }
        match s.recv(&mut buf) {
            Ok(n) => {
                write_pcap_packet(&mut f, &buf[..n])?;
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock { continue; }
                return Err(e.into());
            }
        }
    }
    f.flush()?;
    Ok(())
}

fn write_pcap_global_header(mut f: &File) -> anyhow::Result<()> {
    // PCAP Global Header (little endian)
    let mut hdr = [0u8; 24];
    hdr[0..4].copy_from_slice(&0xA1B2C3D4u32.to_le_bytes());
    hdr[4..6].copy_from_slice(&2u16.to_le_bytes());
    hdr[6..8].copy_from_slice(&4u16.to_le_bytes());
    hdr[8..12].copy_from_slice(&0i32.to_le_bytes());
    hdr[12..16].copy_from_slice(&0u32.to_le_bytes());
    hdr[16..20].copy_from_slice(&65535u32.to_le_bytes());
    hdr[20..24].copy_from_slice(&101u32.to_le_bytes()); // LINKTYPE_RAW (IPv4)
    f.write_all(&hdr)?;
    Ok(())
}

fn write_pcap_packet(mut f: &File, data: &[u8]) -> anyhow::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let mut ph = [0u8; 16];
    ph[0..4].copy_from_slice(&(ts.as_secs() as u32).to_le_bytes());
    ph[4..8].copy_from_slice(&(ts.subsec_nanos() / 1000).to_le_bytes());
    ph[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    ph[12..16].copy_from_slice(&(data.len() as u32).to_le_bytes());
    f.write_all(&ph)?;
    f.write_all(data)?;
    Ok(())
}


