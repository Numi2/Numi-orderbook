// src/net.rs
use crate::config::{ChannelCfg, TimestampingMode};
use anyhow::Context;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

pub fn build_mcast_socket(cfg: &ChannelCfg) -> anyhow::Result<UdpSocket> {
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, cfg.port);
    let sock = UdpSocket::bind(bind_addr).context("bind udp")?;

    // Reuse options
    set_reuse(&sock, cfg.reuse_port)?;

    // Join multicast on the specified iface
    sock.join_multicast_v4(&cfg.group, &cfg.iface_addr).context("join mcast")?;

    // Buffer sizes
    if cfg.recv_buffer_bytes > 0 {
        sock.set_recv_buffer_size(cfg.recv_buffer_bytes as usize).ok();
    }

    // Busy poll (Linux only)
    set_busy_poll(&sock, cfg.busy_poll_us);

    // Timestamping (Linux only)
    set_timestamping(&sock, cfg.timestamping.as_ref());

    // Nonblocking
    sock.set_nonblocking(cfg.nonblocking).ok();

    Ok(sock)
}

fn set_reuse(sock: &UdpSocket, reuse_port: bool) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    unsafe {
        let one: libc::c_int = 1;
        // SO_REUSEADDR
        let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
        // SO_REUSEPORT
        if reuse_port {
            let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, &one as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
        }
    }
    Ok(())
}

fn set_busy_poll(sock: &UdpSocket, busy_poll_us: Option<u32>) {
    #[cfg(target_os = "linux")]
    if let Some(us) = busy_poll_us { unsafe {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        let val: libc::c_int = us as libc::c_int;
        let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BUSY_POLL, &val as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
    }}
}

fn set_timestamping(sock: &UdpSocket, mode: Option<&TimestampingMode>) {
    #[cfg(target_os = "linux")]
    if let Some(m) = mode { unsafe {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        match m {
            TimestampingMode::Off => {
                let zero: libc::c_int = 0;
                let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, &zero as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
            }
            TimestampingMode::Software => {
                let one: libc::c_int = 1;
                let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_TIMESTAMPNS, &one as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
            }
            TimestampingMode::Hardware | TimestampingMode::HardwareRaw => {
                // SO_TIMESTAMPING flags
                const SOF_TIMESTAMPING_RX_HARDWARE: libc::c_int = 1<<0;
                const SOF_TIMESTAMPING_RAW_HARDWARE: libc::c_int = 1<<6;
                const SOF_TIMESTAMPING_SOFTWARE: libc::c_int = 1<<4;
                let mut flags = SOF_TIMESTAMPING_SOFTWARE | SOF_TIMESTAMPING_RX_HARDWARE;
                if matches!(m, TimestampingMode::HardwareRaw) {
                    flags |= SOF_TIMESTAMPING_RAW_HARDWARE;
                }
                let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_TIMESTAMPING, &flags as *const _ as *const _, std::mem::size_of::<libc::c_int>() as _);
            }
        }
    }}
}

// src/net.rs
use crate::config::{ChannelCfg, TimestampingMode};
use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

pub fn build_mcast_socket(cfg: &ChannelCfg) -> anyhow::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("socket")?;

    sock.set_reuse_address(true).ok();
    if cfg.reuse_port {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        sock.set_reuse_port(true).ok();
    }

    // Bind to wildcard:port for multicast RX
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.port);
    sock.bind(&bind_addr.into()).context("bind")?;

    // Increase receive buffer to tolerate bursts
    if cfg.recv_buffer_bytes > 0 {
        let _ = sock.set_recv_buffer_size(cfg.recv_buffer_bytes as usize);
    }

    // Join multicast group on specified interface
    let group = cfg.group;
    let iface = cfg.iface_addr;
    sock.join_multicast_v4(&group, &iface).context("join_multicast_v4")?;

    // Optional busy-poll hint (Linux only)
    if let Some(us) = cfg.busy_poll_us {
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::fd::AsRawFd;
            let fd = sock.as_raw_fd();
            let val: libc::c_int = us as libc::c_int;
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BUSY_POLL,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    // Optional RX timestamping (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(mode) = &cfg.timestamping {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        unsafe {
            match mode {
                TimestampingMode::Off => {}
                TimestampingMode::Software => {
                    // Enable nanosecond software timestamps (simpler path)
                    let on: libc::c_int = 1;
                    let _ = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPNS,
                        &on as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
                TimestampingMode::Hardware | TimestampingMode::HardwareRaw => {
                    // Use SO_TIMESTAMPING and return SCM_TIMESTAMPING (timespec[3])
                    // Choose RAW_HARDWARE when requested, otherwise SYSTEM_HARDWARE.
                    #[allow(non_upper_case_globals)]
                    const RX_SW: libc::c_int = libc::SOF_TIMESTAMPING_RX_SOFTWARE as libc::c_int;
                    #[allow(non_upper_case_globals)]
                    const SW: libc::c_int = libc::SOF_TIMESTAMPING_SOFTWARE as libc::c_int;
                    #[allow(non_upper_case_globals)]
                    const RX_HW: libc::c_int = libc::SOF_TIMESTAMPING_RX_HARDWARE as libc::c_int;
                    #[allow(non_upper_case_globals)]
                    const SYS_HW: libc::c_int = libc::SOF_TIMESTAMPING_SYS_HARDWARE as libc::c_int;
                    #[allow(non_upper_case_globals)]
                    const RAW_HW: libc::c_int = libc::SOF_TIMESTAMPING_RAW_HARDWARE as libc::c_int;
                    let mut flags = RX_SW | SW; // keep software as fallback
                    flags |= RX_HW;
                    flags |= match mode {
                        TimestampingMode::HardwareRaw => RAW_HW,
                        _ => SYS_HW,
                    };
                    let _ = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPING,
                        &flags as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
            }
        }
    }

    let s: UdpSocket = sock.into();
    if cfg.nonblocking {
        s.set_nonblocking(true).ok();
    }
    Ok(s)
}


