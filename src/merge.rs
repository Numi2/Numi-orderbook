// src/merge.rs (updated: metrics + recovery)
use crate::metrics;
use crate::pool::Pkt;
use crate::recovery::RecoveryClient;
use crate::spsc::SpscQueue;
use crate::util::BarrierFlag;
use log::warn;
// Reorder buffer is implemented as a fixed-size ring to minimize allocations and compares
use std::sync::Arc;

pub struct MergeConfig {
    pub next_seq: u64,
    pub reorder_window: u64,
    pub max_pending: usize,
    pub dwell_ns: u64,
    pub adaptive: bool,
    pub reorder_window_max: u64,
}

// TODO: Group arguments into a MergeConfig struct to reduce parameter count.
pub fn merge_loop(
    q_a_list: Vec<Arc<SpscQueue<Pkt>>>,
    q_b_list: Vec<Arc<SpscQueue<Pkt>>>,
    q_out: Arc<SpscQueue<Pkt>>,
    cfg: MergeConfig,
    shutdown: Arc<BarrierFlag>,
    recovery: Option<RecoveryClient>,
    q_recovery_in: Option<Arc<SpscQueue<Pkt>>>, // optional recovery->merge SPSC queue
) -> anyhow::Result<()> {
    let MergeConfig {
        mut next_seq,
        mut reorder_window,
        max_pending,
        dwell_ns,
        adaptive,
        reorder_window_max,
    } = cfg;
    let cap: usize = (reorder_window as usize).saturating_add(1);
    let mut ring: Vec<Option<(u64, Pkt)>> = (0..cap).map(|_| None).collect();
    let mut pending_count: usize = 0;

    // Prefer-A with hysteresis: start preferring A; consider switching based on consecutive
    // non-preferred forwards and a minimum dwell time since the last switch.
    let mut prefer_a: bool = true;
    let mut streak_preferred: u32 = 0;
    let mut streak_nonpreferred: u32 = 0;
    const SWITCH_TO_B_AFTER: u32 = 2; // consecutive non-preferred forwards
    const SWITCH_TO_A_AFTER: u32 = 8; // consecutive preferred forwards to flip back
    let mut last_switch_ns: u64 = crate::util::now_nanos();
    let mut min_dwell_ns: u64 = if dwell_ns == 0 { 2_000_000 } else { dwell_ns };
    metrics::set_merge_preferred_is_a(true);

    // Adaptive window counters
    let mut forwarded_since_check: u64 = 0;
    let mut recent_gaps: u64 = 0;
    let mut recent_ooo: u64 = 0;
    let mut switches_in_window: u32 = 0;

    let mut idx_a: usize = 0;
    let mut idx_b: usize = 0;
    let na = q_a_list.len().max(1);
    let nb = q_b_list.len().max(1);

    while !shutdown.is_raised() {
        let mut moved = false;

        // Drain at most a small batch of recovered packets each loop to avoid starvation
        if let Some(ref qrec) = q_recovery_in {
            // up to 32 per iteration
            for _ in 0..32 {
                if let Some(pkt) = qrec.pop() {
                    let s = pkt.seq;
                    if s < next_seq {
                        metrics::inc_merge_dup();
                    } else if s == next_seq {
                        forward(&q_out, pkt);
                        metrics::inc_merge_forward_chan("R");
                        next_seq = next_seq.wrapping_add(1);
                        moved = true;
                        // Drain contiguous buffered packets
                        loop {
                            let idx = (next_seq % (cap as u64)) as usize;
                            if let Some((stored_seq, node)) = ring[idx].take() {
                                if stored_seq != next_seq {
                                    ring[idx] = Some((stored_seq, node));
                                    break;
                                }
                                pending_count = pending_count.saturating_sub(1);
                                metrics::inc_merge_ooo();
                                let c = if node.chan == b'A' {
                                    "A"
                                } else if node.chan == b'B' {
                                    "B"
                                } else {
                                    "R"
                                };
                                forward(&q_out, node);
                                metrics::inc_merge_forward_chan(c);
                                next_seq = next_seq.wrapping_add(1);
                                forwarded_since_check = forwarded_since_check.saturating_add(1);
                            } else {
                                break;
                            }
                        }
                    } else {
                        let distance = s.wrapping_sub(next_seq);
                        if distance <= reorder_window && pending_count < max_pending {
                            let idx = (s % (cap as u64)) as usize;
                            match &ring[idx] {
                                Some((seq_in_slot, _)) => {
                                    if *seq_in_slot == s {
                                        metrics::inc_merge_dup();
                                    } else if *seq_in_slot < next_seq {
                                        ring[idx] = Some((s, pkt));
                                        pending_count += 1;
                                    } else {
                                        metrics::inc_merge_dup();
                                    }
                                }
                                None => {
                                    ring[idx] = Some((s, pkt));
                                    pending_count += 1;
                                }
                            }
                        } else {
                            metrics::inc_merge_gap();
                            recent_gaps = recent_gaps.saturating_add(1);
                            metrics::inc_merge_gap_chan("R");
                            if let Some(ref cli) = recovery {
                                if s > next_seq {
                                    cli.notify_gap(next_seq, s - 1);
                                }
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        }

        // Try preferred first, then the other (round-robin across workers)
        for src in 0..2 {
            let take_a_first = (src == 0) == prefer_a;
            let pkt = if take_a_first {
                let q = &q_a_list[idx_a % na];
                idx_a = idx_a.wrapping_add(1);
                q.pop()
            } else {
                let q = &q_b_list[idx_b % nb];
                idx_b = idx_b.wrapping_add(1);
                q.pop()
            };
            if let Some(pkt) = pkt {
                let s = pkt.seq;
                if s < next_seq {
                    metrics::inc_merge_dup();
                    continue;
                }
                if s == next_seq {
                    let chan = if pkt.chan == b'A' { "A" } else { "B" };
                    forward(&q_out, pkt);
                    metrics::inc_merge_forward_chan(chan);
                    next_seq = next_seq.wrapping_add(1);
                    moved = true;
                    // Drain contiguous buffered packets
                    loop {
                        let idx = (next_seq % (cap as u64)) as usize;
                        if let Some((stored_seq, node)) = ring[idx].take() {
                            if stored_seq != next_seq {
                                // stale/aliased entry; drop it and stop draining
                                ring[idx] = Some((stored_seq, node));
                                break;
                            }
                            pending_count = pending_count.saturating_sub(1);
                            metrics::inc_merge_ooo();
                            recent_ooo = recent_ooo.saturating_add(1);
                            let c = if node.chan == b'A' { "A" } else { "B" };
                            forward(&q_out, node);
                            metrics::inc_merge_forward_chan(c);
                            next_seq = next_seq.wrapping_add(1);
                            forwarded_since_check = forwarded_since_check.saturating_add(1);
                        } else {
                            break;
                        }
                    }

                    // Hysteresis update: observe source vs preference
                    let was_a = chan == "A";
                    let is_preferred_src = (prefer_a && was_a) || (!prefer_a && !was_a);
                    if is_preferred_src {
                        streak_preferred = streak_preferred.saturating_add(1);
                        streak_nonpreferred = 0;
                        if !prefer_a
                            && streak_preferred >= SWITCH_TO_A_AFTER
                            && crate::util::now_nanos().saturating_sub(last_switch_ns)
                                >= min_dwell_ns
                        {
                            prefer_a = true;
                            streak_preferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(true);
                            last_switch_ns = crate::util::now_nanos();
                            switches_in_window = switches_in_window.saturating_add(1);
                        }
                    } else {
                        streak_nonpreferred = streak_nonpreferred.saturating_add(1);
                        streak_preferred = 0;
                        if prefer_a
                            && streak_nonpreferred >= SWITCH_TO_B_AFTER
                            && crate::util::now_nanos().saturating_sub(last_switch_ns)
                                >= min_dwell_ns
                        {
                            prefer_a = false;
                            streak_nonpreferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(false);
                            last_switch_ns = crate::util::now_nanos();
                            switches_in_window = switches_in_window.saturating_add(1);
                        }
                    }
                } else {
                    let distance = s.wrapping_sub(next_seq);
                    if distance <= reorder_window && pending_count < max_pending {
                        let idx = (s % (cap as u64)) as usize;
                        match &ring[idx] {
                            Some((seq_in_slot, _)) => {
                                if *seq_in_slot == s {
                                    metrics::inc_merge_dup();
                                } else if *seq_in_slot < next_seq {
                                    // stale slot from an old window; replace
                                    ring[idx] = Some((s, pkt));
                                    pending_count += 1;
                                } else {
                                    // different seq still in-window shouldn't alias due to cap, but guard anyway
                                    metrics::inc_merge_dup();
                                }
                            }
                            None => {
                                ring[idx] = Some((s, pkt));
                                pending_count += 1;
                            }
                        }
                    } else {
                        metrics::inc_merge_gap();
                        recent_gaps = recent_gaps.saturating_add(1);
                        let chan = if pkt.chan == b'A' { "A" } else { "B" };
                        metrics::inc_merge_gap_chan(chan);
                        warn!(
                            "gap/overflow: got seq={}, expected={}, pending={}, window={}, from={}",
                            s, next_seq, pending_count, reorder_window, chan
                        );
                        if let Some(ref cli) = recovery {
                            if s > next_seq {
                                cli.notify_gap(next_seq, s - 1);
                            }
                        }
                    }
                }
            }
        }

        // Adaptive window adjustment checkpoint
        if adaptive && forwarded_since_check >= 4096 {
            if recent_gaps > 0 && reorder_window < reorder_window_max {
                let grow_by = (reorder_window / 4).max(1);
                reorder_window = (reorder_window + grow_by).min(reorder_window_max);
            }
            if recent_ooo == 0 && recent_gaps == 0 && reorder_window > 8 {
                reorder_window = (reorder_window.saturating_sub(reorder_window / 8)).max(8);
            }
            // Adapt dwell if we ping-pong too often
            if switches_in_window >= 4 {
                min_dwell_ns = (min_dwell_ns.saturating_mul(2)).min(50_000_000);
            // cap at 50ms
            } else if switches_in_window == 0 && min_dwell_ns > dwell_ns {
                // decay
                min_dwell_ns = (min_dwell_ns.saturating_sub(min_dwell_ns / 4)).max(dwell_ns);
            }
            forwarded_since_check = 0;
            recent_gaps = 0;
            recent_ooo = 0;
            switches_in_window = 0;
        }

        if !moved {
            crate::util::spin_wait(32);
        }
    }

    Ok(())
}

#[inline]
fn forward(q_out: &Arc<SpscQueue<Pkt>>, mut pkt: Pkt) {
    // Stage timing and mark merge emit time
    let now = crate::util::now_nanos();
    if pkt.ts_nanos != 0 && now > pkt.ts_nanos {
        metrics::observe_stage_rx_to_merge_ns(now - pkt.ts_nanos);
    }
    pkt.merge_emit_ns = now;
    let mut retries: u32 = 0;
    loop {
        match q_out.push_with_backoff(pkt, 1024) {
            Ok(()) => break,
            Err(returned) => {
                pkt = returned;
                retries = retries.wrapping_add(1);
                if (retries & 0x3f) == 0 {
                    warn!(
                        "merge->decode backpressure: retries={} q_len~{}",
                        retries,
                        q_out.len()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn pkt(seq: u64, chan: u8) -> Pkt {
        Pkt {
            buf: crate::pool::PktBuf::Bytes(BytesMut::new()),
            len: 0,
            seq,
            ts_nanos: 0,
            chan,
            _ts_kind: crate::pool::TsKind::Sw,
            merge_emit_ns: 0,
        }
    }

    #[test]
    fn merge_reorders_in_window_and_drops_dupes() {
        let q_a: Arc<SpscQueue<Pkt>> = Arc::new(SpscQueue::new(64));
        let q_b: Arc<SpscQueue<Pkt>> = Arc::new(SpscQueue::new(64));
        let q_out: Arc<SpscQueue<Pkt>> = Arc::new(SpscQueue::new(256));
        let shutdown = Arc::new(crate::util::BarrierFlag::default());

        let qa = q_a.clone();
        let qb = q_b.clone();
        let qo = q_out.clone();
        let sd = shutdown.clone();
        let t = std::thread::spawn(move || {
            let cfg = MergeConfig {
                next_seq: 1,
                reorder_window: 4,
                max_pending: 64,
                dwell_ns: 0,
                adaptive: false,
                reorder_window_max: 8,
            };
            let _ = merge_loop(vec![qa], vec![qb], qo, cfg, sd, None, None);
        });

        // Feed out-of-order within window and duplicates across channels
        let _ = q_a.push(pkt(1, b'A'));
        let _ = q_b.push(pkt(3, b'B'));
        let _ = q_a.push(pkt(2, b'A'));
        let _ = q_b.push(pkt(2, b'B')); // duplicate
        let _ = q_a.push(pkt(4, b'A'));

        // wait until we see at least 4 outputs or timeout
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while q_out.len() < 4 && std::time::Instant::now() < deadline {
            crate::util::spin_wait(1000);
        }
        shutdown.raise();
        let _ = t.join();

        let mut seqs = Vec::new();
        while let Some(p) = q_out.pop() {
            seqs.push(p.seq);
        }
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }
}
