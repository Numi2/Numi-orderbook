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
    let mut ring: Vec<Option<Pkt>> = vec![None; cap];
    let mut pending_count: usize = 0;

    // Prefer-A with hysteresis: start preferring A; switch if we forward
    // a certain number of consecutive non-preferred packets.
    let mut prefer_a: bool = true;
    let mut streak_preferred: u32 = 0;
    let mut streak_nonpreferred: u32 = 0;
    const SWITCH_TO_B_AFTER: u32 = 2; // consecutive non-preferred forwards
    const SWITCH_TO_A_AFTER: u32 = 8; // consecutive preferred forwards to flip back
    metrics::set_merge_preferred_is_a(true);

    while !shutdown.is_raised() {
        let mut moved = false;

        // Try preferred first, then the other
        for src in 0..2 {
            let take_a_first = (src == 0) == prefer_a;
            let pkt = if take_a_first { q_a.pop() } else { q_b.pop() };
            if let Some(pkt) = pkt {
                let s = pkt.seq;
                if std::hint::unlikely(s < next_seq) {
                    metrics::inc_merge_dup();
                    continue;
                }
                if std::hint::likely(s == next_seq) {
                    let chan = if pkt.chan == b'A' { "A" } else { "B" };
                    forward(&q_out, pkt)?;
                    metrics::inc_merge_forward_chan(chan);
                    next_seq = next_seq.wrapping_add(1);
                    moved = true;
                    // Drain contiguous buffered packets
                    loop {
                        let idx = (next_seq % (cap as u64)) as usize;
                        if let Some(node) = ring[idx].take() {
                            pending_count = pending_count.saturating_sub(1);
                            metrics::inc_merge_ooo();
                            let c = if node.chan == b'A' { "A" } else { "B" };
                            forward(&q_out, node)?;
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
                            prefer_a = true;
                            streak_preferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(true);
                        }
                    } else {
                        streak_nonpreferred = streak_nonpreferred.saturating_add(1);
                        streak_preferred = 0;
                        if prefer_a && streak_nonpreferred >= SWITCH_TO_B_AFTER {
                            prefer_a = false;
                            streak_nonpreferred = 0;
                            metrics::inc_merge_failover();
                            metrics::set_merge_preferred_is_a(false);
                        }
                    }
                } else {
                    let distance = s.wrapping_sub(next_seq);
                    if std::hint::likely(distance <= reorder_window) && pending_count < max_pending {
                        let idx = (s % (cap as u64)) as usize;
                        if ring[idx].is_some() {
                            metrics::inc_merge_dup();
                        } else {
                            ring[idx] = Some(pkt);
                            pending_count += 1;
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
fn forward(q_out: &Arc<ArrayQueue<Pkt>>, pkt: Pkt) -> anyhow::Result<()> {
    q_out.push(pkt).map_err(|_| anyhow::anyhow!("merge output queue full"))
}