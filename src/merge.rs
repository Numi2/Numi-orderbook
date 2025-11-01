// src/merge.rs (updated: metrics + recovery)
use crate::metrics;
use crate::pool::Pkt;
use crate::recovery::Client as RecoveryClient;
use crate::util::BarrierFlag;
use crossbeam::queue::ArrayQueue;
use log::{warn};
use std::collections::BTreeMap;
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
    let mut pending: BTreeMap<u64, Pkt> = BTreeMap::new();

    while !shutdown.is_raised() {
        let mut moved = false;

        for src in 0..2 {
            let pkt = if src == 0 { q_a.pop() } else { q_b.pop() };
            if let Some(pkt) = pkt {
                let s = pkt.seq;
                if std::hint::unlikely(s < next_seq) {
                    metrics::inc_merge_dup();
                    continue;
                }
                if std::hint::likely(s == next_seq) {
                    forward(&q_out, pkt)?;
                    next_seq = next_seq.wrapping_add(1);
                    moved = true;
                    while let Some(node) = pending.remove(&next_seq) {
                        metrics::inc_merge_ooo();
                        forward(&q_out, node)?;
                        next_seq = next_seq.wrapping_add(1);
                    }
                } else {
                    if std::hint::likely(s.wrapping_sub(next_seq) <= reorder_window) && pending.len() < max_pending {
                        let replaced = pending.insert(s, pkt);
                        if replaced.is_some() {
                            metrics::inc_merge_dup();
                        }
                    } else {
                        metrics::inc_merge_gap();
                        warn!(
                            "gap/overflow: got seq={}, expected={}, pending={}, window={}",
                            s, next_seq, pending.len(), reorder_window
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