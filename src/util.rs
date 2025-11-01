// src/util.rs
use std::sync::atomic::{AtomicBool, Ordering};

pub struct BarrierFlag(AtomicBool);

impl Default for BarrierFlag {
    fn default() -> Self { Self(AtomicBool::new(false)) }
}

impl BarrierFlag {
    #[inline]
    pub fn raise(&self) { self.0.store(true, Ordering::SeqCst); }
    #[inline]
    pub fn is_raised(&self) -> bool { self.0.load(Ordering::Relaxed) }
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
    // Fallback portable monotonic
    use std::time::Instant;
    static START: once_cell::sync::Lazy<Instant> = once_cell::sync::Lazy::new(Instant::now);
    START.elapsed().as_nanos() as u64
}

#[inline]
pub fn lock_all_memory_if(cfg: bool) {
    if !cfg {}
    #[cfg(target_os = "linux")]
    unsafe {
        // Best-effort raise RLIMIT_MEMLOCK
        let mut lim = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
        let _ = libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim);
        let flags = libc::MCL_CURRENT | libc::MCL_FUTURE;
        let _ = libc::mlockall(flags);
    }
}

#[inline]
pub fn set_realtime_priority_if(_priority: Option<i32>) {
    #[cfg(target_os = "linux")]
    if let Some(pri) = _priority {
        unsafe {
            let param = libc::sched_param { sched_priority: pri as i32 };
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
    std::fs::read_to_string(path).ok()?.trim().parse::<i32>().ok()
}

pub fn node_cpulist(node: i32) -> Option<String> {
    let path = format!("/sys/devices/system/node/node{}/cpulist", node);
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn cpulist_contains(cpulist: &str, cpu_id: usize) -> bool {
    // Parse cpulist format like "0-3,8,10-11"
    for part in cpulist.split(',') {
        let part = part.trim();
        if part.is_empty() { continue; }
        if let Some((a,b)) = part.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (a.parse::<usize>(), b.parse::<usize>()) {
                if cpu_id >= lo && cpu_id <= hi { return true; }
            }
        } else if let Ok(v) = part.parse::<usize>() {
            if v == cpu_id { return true; }
        }
    }
    false
}
