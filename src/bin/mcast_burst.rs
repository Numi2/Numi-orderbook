use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 7 {
        eprintln!(
            "usage: mcast_burst <group> <port> <iface_ipv4> <payload_size> <packets> <rate_pps>"
        );
        std::process::exit(2);
    }
    let group: Ipv4Addr = args[1].parse()?;
    let port: u16 = args[2].parse()?;
    let iface: Ipv4Addr = args[3].parse()?;
    let payload_size: usize = args[4].parse()?;
    let packets: u64 = args[5].parse()?;
    let rate_pps: u64 = args[6].parse()?;

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true).ok();
    sock.bind(&SocketAddr::new(IpAddr::V4(iface), 0).into())?;
    sock.set_multicast_loop_v4(false)?;
    sock.set_multicast_ttl_v4(1)?;
    sock.join_multicast_v4(&group, &iface)?;

    let dest = SocketAddr::new(IpAddr::V4(group), port);
    let mut buf = vec![0u8; payload_size];
    let nanos_per_pkt = if rate_pps == 0 {
        0
    } else {
        1_000_000_000u64 / rate_pps
    };
    let start = std::time::Instant::now();
    for i in 0..packets {
        // simple incrementing sequence at start of payload (big-endian u64)
        if payload_size >= 8 {
            buf[0..8].copy_from_slice(&i.to_be_bytes());
        }
        let _ = sock.send_to(&buf, &dest.into());
        if nanos_per_pkt > 0 {
            busy_sleep_nanos(nanos_per_pkt);
        }
    }
    eprintln!("sent {} packets in {:?}", packets, start.elapsed());
    Ok(())
}

#[inline]
fn busy_sleep_nanos(ns: u64) {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_nanos() as u64 >= ns {
            break;
        }
        std::hint::spin_loop();
    }
}
