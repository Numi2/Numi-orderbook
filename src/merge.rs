// src/merge.rs (updated: metrics + recovery)
use crate::metrics;
use crate::pool::Pkt;
use crate::recovery::Client as RecoveryClient;
use crate::util::BarrierFlag;
use crossbeam::queue::ArrayQueue;
use log::{warn};
// Reorder buffer is implemented as a fixed-size ring to minimize allocations and compares
use std::sync::Arc;

pub fn merge_loop(
    q_a: Arc<ArrayQueue<Pkt>>,
    q_b: Arc<ArrayQueue<Pkt>>,
    q_out: Arc<ArrayQueue<Pkt>>,
    _seq: std::sync::Arc<dyn crate::parser::SeqExtractor>,
    mut next_seq: u64,
    reorder_window: u64,
    max_pending: usize,
    shutdown: Arc<BarrierFlag>,
    recovery: Option<RecoveryClient>,
) -> anyhow::Result<()> {
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
    const MIN_DWELL_NS: u64 = 2_000_000; // 2ms dwell before allowing another switch
    metrics::set_merge_preferred_is_a(true);

    while !shutdown.is_raised() {
        let mut moved = false;

        // Try preferred first, then the other
        for src in 0..2 {
            let take_a_first = (src == 0) == prefer_a;
            let pkt = if take_a_first { q_a.pop() } else { q_b.pop() };
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
                            let c = if node.chan == b'A' { "A" } else { "B" };
                            forward(&q_out, node);
                            metrics::inc_merge_forward_chan(c);
                            next_seq = next_seq.wrapping_add(1);
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
                        if !prefer_a && streak_preferred >= SWITCH_TO_A_AFTER {
                            if crate::util::now_nanos().saturating_sub(last_switch_ns) >= MIN_DWELL_NS {
                            prefer_a = true;
                            streak_preferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(true);
                                last_switch_ns = crate::util::now_nanos();
                            }
                        }
                    } else {
                        streak_nonpreferred = streak_nonpreferred.saturating_add(1);
                        streak_preferred = 0;
                        if prefer_a && streak_nonpreferred >= SWITCH_TO_B_AFTER {
                            if crate::util::now_nanos().saturating_sub(last_switch_ns) >= MIN_DWELL_NS {
                            prefer_a = false;
                            streak_nonpreferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(false);
                                last_switch_ns = crate::util::now_nanos();
                            }
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

        if !moved {
            crate::util::spin_wait(32);
        }
    }

    Ok(())
}

#[inline]
fn forward(q_out: &Arc<ArrayQueue<Pkt>>, mut pkt: Pkt) {
    // Stage timing and mark merge emit time
    let now = crate::util::now_nanos();
    if pkt.ts_nanos != 0 && now > pkt.ts_nanos {
        metrics::observe_stage_rx_to_merge_ns(now - pkt.ts_nanos);
    }
    pkt.merge_emit_ns = now;
    loop {
        match q_out.push(pkt) {
            Ok(()) => break,
            Err(ret) => {
                pkt = ret;
                crate::util::spin_wait(32);
            }
        }
    }
}