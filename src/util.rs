// src/util.rs
use std::sync::atomic::{AtomicBool, Ordering};

pub struct BarrierFlag(AtomicBool);

impl Default for BarrierFlag {
    fn default() -> Self {
        Self(AtomicBool::new(false))
    }
}

impl BarrierFlag {
    #[inline]
    pub fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    #[inline]
    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[inline]
pub fn spin_wait(mut loops: u32) {
    while loops > 0 {
        std::hint::spin_loop();
        loops -= 1;
    }
}

#[inline]
pub fn pin_to_core_if_set(core_index: Option<usize>) {
    if let Some(idx) = core_index {
        if let Some(cores) = core_affinity::get_core_ids() {
            if let Some(core_id) = cores.into_iter().find(|c| c.id == idx) {
                let _ = core_affinity::set_for_current(core_id);
            }
        }
    }
}

#[inline]
pub fn pin_to_core_with_offset(base_core_index: Option<usize>, offset: usize) {
    if let Some(base) = base_core_index {
        if let Some(cores) = core_affinity::get_core_ids() {
            let target = base.saturating_add(offset);
            if let Some(core_id) = cores.into_iter().find(|c| c.id == target) {
                let _ = core_affinity::set_for_current(core_id);
            }
        }
    }
}

#[inline]
pub fn now_nanos() -> u64 {
    #[cfg(target_os = "linux")]
    {
        use nix::time::{clock_gettime, ClockId};
        if let Ok(ts) = clock_gettime(ClockId::CLOCK_MONOTONIC_RAW) {
            return (ts.tv_sec() as u64) * 1_000_000_000 + (ts.tv_nsec() as u64);
        }
    }
    #[cfg(target_os = "macos")]
    {
        mach_absolute_to_nanos(mach_absolute_time_ticks())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Fallback portable monotonic
        use std::time::Instant;
        static START: once_cell::sync::Lazy<Instant> = once_cell::sync::Lazy::new(Instant::now);
        START.elapsed().as_nanos() as u64
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

#[cfg(target_os = "macos")]
#[inline]
pub fn mach_absolute_time_ticks() -> u64 {
    unsafe { mach_absolute_time() }
}

#[cfg(target_os = "macos")]
#[inline]
pub fn mach_absolute_to_nanos(ticks: u64) -> u64 {
    static TIMEBASE: once_cell::sync::Lazy<(u64, u64)> = once_cell::sync::Lazy::new(|| unsafe {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        let rc = mach_timebase_info(&mut info);
        if rc != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer as u64, info.denom as u64)
        }
    });
    let (numer, denom) = *TIMEBASE;
    if numer == denom {
        return ticks;
    }
    if denom == 1 {
        return ticks.saturating_mul(numer);
    }
    ((ticks as u128).saturating_mul(numer as u128) / denom as u128).min(u64::MAX as u128) as u64
}

#[inline]
pub fn lock_all_memory_if(cfg: bool) -> anyhow::Result<()> {
    if !cfg {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    unsafe {
        let lim = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        if libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) != 0 {
            anyhow::bail!(
                "setrlimit(RLIMIT_MEMLOCK) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let flags = libc::MCL_CURRENT | libc::MCL_FUTURE;
        if libc::mlockall(flags) != 0 {
            anyhow::bail!(
                "mlockall(MCL_CURRENT|MCL_FUTURE) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("general.mlock_all is only supported on Linux");
    }
}

#[inline]
pub fn set_realtime_priority_if(_priority: Option<i32>) {
    #[cfg(target_os = "linux")]
    if let Some(pri) = _priority {
        unsafe {
            let param = libc::sched_param {
                sched_priority: pri,
            };
            let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        }
    }
}

// Adaptive idle: escalate from spin -> yield -> short sleep to reduce CPU when idle
#[inline]
pub fn adaptive_wait(idle_iters: &mut u32, base_spins: u32) {
    if *idle_iters < 64 {
        spin_wait(base_spins);
        *idle_iters += 1;
    } else if *idle_iters < 256 {
        std::thread::yield_now();
        *idle_iters += 1;
    } else {
        // small sleep; keeps latency reasonable while avoiding 100% CPU when idle
        std::thread::sleep(std::time::Duration::from_micros(50));
        *idle_iters = 256; // clamp
    }
}

// -------- NUMA helpers (best-effort without extra deps) --------
pub fn iface_numa_node(ifname: &str) -> Option<i32> {
    let path = format!("/sys/class/net/{}/device/numa_node", ifname);
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

pub fn node_cpulist(node: i32) -> Option<String> {
    let path = format!("/sys/devices/system/node/node{}/cpulist", node);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn cpulist_contains(cpulist: &str, cpu_id: usize) -> bool {
    // Parse cpulist format like "0-3,8,10-11"
    for part in cpulist.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (a.parse::<usize>(), b.parse::<usize>()) {
                if cpu_id >= lo && cpu_id <= hi {
                    return true;
                }
            }
        } else if let Ok(v) = part.parse::<usize>() {
            if v == cpu_id {
                return true;
            }
        }
    }
    false
}

// These functions are used in main.rs for NUMA validation
