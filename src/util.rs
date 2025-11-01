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
    if !cfg { return; }
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
pub fn set_realtime_priority_if(priority: Option<i32>) {
    #[cfg(target_os = "linux")]
    if let Some(pri) = priority {
        unsafe {
            let param = libc::sched_param { sched_priority: pri as i32 };
            let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        }
    }
}


