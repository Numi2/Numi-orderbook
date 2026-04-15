// src/net.rs
use crate::config::ChannelCfg;
use anyhow::Context;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

pub fn build_mcast_socket(cfg: &ChannelCfg) -> anyhow::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).context("socket")?;

    sock.set_reuse_address(true)
        .context("set SO_REUSEADDR on multicast socket")?;
    if cfg.reuse_port {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        sock.set_reuse_port(true)
            .context("set SO_REUSEPORT on multicast socket")?;
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
        anyhow::bail!("reuse_port is requested but is not supported on this target");
    }

    // Bind to wildcard:port for multicast RX
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.port);
    sock.bind(&bind_addr.into()).context("bind")?;

    // Increase receive buffer to tolerate bursts
    if cfg.recv_buffer_bytes > 0 {
        let requested = cfg.recv_buffer_bytes as usize;
        sock.set_recv_buffer_size(requested)
            .with_context(|| format!("set socket receive buffer to {requested} bytes"))?;
        let actual = sock
            .recv_buffer_size()
            .context("read socket receive buffer size")?;
        if actual < requested {
            anyhow::bail!(
                "socket receive buffer below requested size: requested={} actual={}",
                requested,
                actual
            );
        }
    }

    // Join multicast group on specified interface
    let group = cfg.group;
    let iface = cfg.iface_addr;
    sock.join_multicast_v4(&group, &iface)
        .context("join_multicast_v4")?;

    // Optional busy-poll hint (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(us) = cfg.busy_poll_us {
        unsafe {
            use std::os::fd::AsRawFd;
            let fd = sock.as_raw_fd();
            let val: libc::c_int = us as libc::c_int;
            let rc = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BUSY_POLL,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("set SO_BUSY_POLL to {us}us"));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    if cfg.busy_poll_us.is_some() {
        anyhow::bail!("busy_poll_us is configured but SO_BUSY_POLL is only supported on Linux");
    }

    // Optional RX timestamping (Linux only)
    #[cfg(target_os = "linux")]
    if let Some(mode) = &cfg.timestamping {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        unsafe {
            match mode {
                crate::config::TimestampingMode::Off => {}
                crate::config::TimestampingMode::Software => {
                    // Enable nanosecond software timestamps (simpler path)
                    let on: libc::c_int = 1;
                    let rc = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPNS,
                        &on as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error())
                            .context("set SO_TIMESTAMPNS on multicast socket");
                    }
                }
                crate::config::TimestampingMode::Hardware
                | crate::config::TimestampingMode::HardwareRaw => {
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
                        crate::config::TimestampingMode::HardwareRaw => RAW_HW,
                        _ => SYS_HW,
                    };
                    let rc = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPING,
                        &flags as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error())
                            .context("set SO_TIMESTAMPING on multicast socket");
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(mode) = &cfg.timestamping {
        if !matches!(mode, crate::config::TimestampingMode::Off) {
            anyhow::bail!(
                "timestamping={mode:?} is configured but RX timestamping is only implemented on Linux"
            );
        }
    }

    let s: UdpSocket = sock.into();
    if cfg.nonblocking {
        s.set_nonblocking(true).ok();
    }
    Ok(s)
}
