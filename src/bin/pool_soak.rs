use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

#[path = "../pool.rs"]
mod pool;

mod metrics {
    use super::{AtomicU64, AtomicUsize, Ordering};

    static POOL_MISSES: AtomicU64 = AtomicU64::new(0);
    static POOL_RETURN_DROPS: AtomicU64 = AtomicU64::new(0);
    static POOL_PREALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

    pub fn inc_packet_pool_miss() {
        POOL_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_packet_pool_return_drop() {
        POOL_RETURN_DROPS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_packet_pool_preallocated_bytes(n: usize) {
        POOL_PREALLOC_BYTES.store(n, Ordering::Relaxed);
    }

    pub fn packet_pool_misses() -> u64 {
        POOL_MISSES.load(Ordering::Relaxed)
    }

    pub fn packet_pool_return_drops() -> u64 {
        POOL_RETURN_DROPS.load(Ordering::Relaxed)
    }

    pub fn packet_pool_preallocated_bytes() -> usize {
        POOL_PREALLOC_BYTES.load(Ordering::Relaxed)
    }
}

use pool::PacketPool;

fn parse_arg_usize(args: &[String], idx: usize, default: usize) -> usize {
    args.get(idx)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
}

fn parse_arg_bool(args: &[String], idx: usize, default: bool) -> bool {
    args.get(idx)
        .map(|s| matches!(s.as_str(), "1" | "true" | "yes" | "allow"))
        .unwrap_or(default)
}

fn main() -> anyhow::Result<()> {
    // Args: [pool_size] [packet_size] [iterations] [burst] [allow_misses]
    let args: Vec<String> = std::env::args().collect();
    let pool_size = parse_arg_usize(&args, 1, 65_536);
    let packet_size = parse_arg_usize(&args, 2, 2_048);
    let iterations = parse_arg_usize(&args, 3, 1_000_000);
    let burst = parse_arg_usize(&args, 4, 64);
    let allow_misses = parse_arg_bool(&args, 5, false);

    let pool = PacketPool::new(pool_size, packet_size)?;
    let mut held = Vec::with_capacity(burst);
    let start = Instant::now();
    let mut ops = 0usize;

    for _ in 0..iterations {
        for _ in 0..burst {
            held.push(pool.get());
        }
        ops = ops.saturating_add(held.len());
        while let Some(buf) = held.pop() {
            pool.put(buf);
        }
    }

    let elapsed = start.elapsed();
    let misses = metrics::packet_pool_misses();
    let return_drops = metrics::packet_pool_return_drops();
    println!(
        "pool_soak: pool_size={} packet_size={} iterations={} burst={} ops={} elapsed_ms={:.3} ops_per_sec={:.3} preallocated_bytes={} misses={} return_drops={}",
        pool_size,
        packet_size,
        iterations,
        burst,
        ops,
        elapsed.as_secs_f64() * 1000.0,
        (ops as f64) / elapsed.as_secs_f64(),
        metrics::packet_pool_preallocated_bytes(),
        misses,
        return_drops,
    );

    if !allow_misses && (misses > 0 || return_drops > 0) {
        anyhow::bail!(
            "packet pool soak failed: misses={} return_drops={} (increase pool_size or lower burst)",
            misses,
            return_drops
        );
    }

    Ok(())
}
