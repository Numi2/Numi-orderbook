Participant Insight Signals
===========================

This layer turns anonymous L3 order flow into explainable market-participant
behavior signals. It must stay outside the ingest, merge, decode, and book
apply hot path. The production orderbook remains the source of truth; insight
detectors consume OBO frames from a sidecar or downstream service.

Signal 1: Absorption
--------------------

Absorption means aggressive flow is pressing into one side of the book while
passive liquidity at the same price holds or replenishes.

Examples:
- Bid absorption: aggressive sellers execute against bid liquidity, but the bid
  remains visible or refills.
- Ask absorption: aggressive buyers execute against ask liquidity, but the ask
  remains visible or refills.

The detector is evidence-first. Each signal includes:
- instrument id
- price
- passive side and aggressor side
- observation window
- executed quantity
- replenished quantity
- pulled quantity
- visible quantity after the latest event
- execute, replenish, and pull event counts
- replenishment and pull ratios
- confidence in basis points

Default thresholds are conservative and configurable:
- 2 second rolling window
- at least 100 executed units
- at least 2 execution events
- either meaningful replenishment or enough visible quantity remaining
- low pull ratio after pressure
- per-level cooldown to avoid alert spam

The detector intentionally does not claim participant identity. Without
venue-provided attribution, it infers anonymous behavior at a price level:
"passive liquidity is absorbing aggressive flow", not "participant X is buying".

Implementation notes:
- Input is normalized raw-v1 OBO events. The module also exposes a raw-v1 frame
  parser so a sidecar can consume the existing WebSocket feed directly.
- The detector keeps its own lightweight order index so qty-only modifies,
  cancels, and executions can be assigned back to the correct level.
- Replenishment and pull events only count after execution pressure exists in
  the rolling window. Adds before pressure do not manufacture absorption.
- Unknown maker-order executions can still seed pressure by instrument, price,
  and opposite aggressor side; later same-level replenishment can confirm the
  signal.
- Duplicate add replacement updates visible state without counting the removal
  as a behavioral pull or the replacement as behavioral replenishment.

Signal 2: Iceberg/Replenishment Candidate
-----------------------------------------

An iceberg/replenishment candidate means aggressive flow repeatedly trades at a
price while visible passive liquidity refills enough times that cumulative
executed quantity exceeds the displayed quantity observed during the window.

Examples:
- Bid-side candidate: aggressive sellers keep hitting the bid, but the bid
  refills at the same price across multiple cycles.
- Ask-side candidate: aggressive buyers keep lifting the ask, but the ask
  refreshes at the same price across multiple cycles.

The detector is deliberately conservative. Each signal includes:
- instrument id
- price
- passive side and aggressor side
- observation window
- executed quantity
- replenished quantity
- pulled quantity
- visible quantity after the latest event
- max visible quantity observed during the window
- execute, replenish, and pull event counts
- replenishment, over-display, and pull ratios
- confidence in basis points

Default thresholds require:
- 5 second rolling window
- at least 100 executed units
- at least 3 execution events
- at least 2 replenish events
- at least 75 replenished units
- executed quantity at least 1.25x max visible quantity
- low pull ratio after pressure
- per-level cooldown to avoid repeated alerts on the same candidate

The detector calls this a candidate because reserve/iceberg behavior cannot be
proven from anonymous public L3 alone. It identifies repeated same-price
replenishment under execution pressure.

Signal 3: Liquidity Pull
------------------------

A liquidity pull means displayed size at a price is withdrawn by cancels or
quantity-reducing modifies before it trades. This is the opposite shape from
absorption: visible interest thins out instead of holding under pressure.

Examples:
- Ask pull: displayed ask size is cancelled or reduced, leaving less supply
  visible above the market.
- Bid pull: displayed bid size is cancelled or reduced, leaving less demand
  visible below the market.

The detector does not treat executions as liquidity pulls. Executions are
tracked as context and can suppress the signal when trading, not withdrawal, is
the dominant reason the level disappeared.

Each signal includes:
- instrument id
- price
- pulled side and opposing side
- observation window
- pulled quantity
- executed quantity
- replenished quantity
- visible quantity after the latest event
- max visible quantity observed during the window
- pull, execute, and replenish event counts
- pull, execution, and visible-after ratios
- confidence in basis points

Default thresholds require:
- 1 second rolling window
- at least 100 pulled units
- at least 2 pull events
- at least 100 visible units observed at the level
- at least 50% of observed visible size pulled
- executions no more than 25% of pulled quantity
- visible quantity after the pull no more than 50% of observed visible size
- per-level cooldown to avoid repeated alerts on the same pull

The signal says displayed liquidity was withdrawn. It does not claim spoofing,
intent, or participant identity without venue-provided attribution.

Sidecar/API
-----------

Run the absorption sidecar against one or two raw-v1 OBO WebSocket endpoints:

```bash
cargo run --release --bin absorption_sidecar -- \
  --url 'ws://127.0.0.1:7001/ws?channel=obo&codec=raw-v1&snapshot=1' \
  --url 'ws://127.0.0.1:7002/ws?channel=obo&codec=raw-v1&snapshot=1' \
  --listen 127.0.0.1:9201
```

The sidecar dedupes live A/B frames by per-instrument OBO sequence, feeds the
absorption, iceberg, and liquidity pull detectors, prints each signal as one
tagged JSON line,
and serves:

- `GET /healthz`: process liveness.
- `GET /ready`: ready after at least one upstream frame has been received.
- `GET /stats`: counters for frames, parsed events, duplicates, parse errors,
  connection attempts, emitted signals, and a diagnostics snapshot.
- `GET /diagnostics`: session-level signal health and regime diagnostics:
  signal counts, max/average score, first/last signal timestamps, and top
  scoring components by total contribution.
- `GET /features`: latest z-scored microstructure feature snapshot per
  instrument when the sidecar is started with `--enable-features`, including
  book imbalance, OFI, touch activity, volume, momentum, slope, and day-of-week
  context.
- `GET /signals`: recent retained participant signals.
- `GET /signals/absorption`: recent retained absorption signals.
- `GET /signals/iceberg`: recent retained iceberg/replenishment candidates.
- `GET /signals/liquidity_pull`: recent retained liquidity pull candidates.

The diagnostics regime is intentionally conservative:

- `balance`: no participant insight signals have fired for the retained session.
- `absorption`: absorption or iceberg/replenishment evidence dominates.
- `spoof_risk`: liquidity-pull signals dominate with high score. This is a risk
  label only; it does not claim illegal spoofing intent.
- `initiative_flow`: signals are present, but neither passive absorption nor
  liquidity-pull risk dominates.

Microstructure Feature Snapshots
--------------------------------

`GET /features` returns the latest feature snapshot per instrument when feature
collection is enabled with `--enable-features`. Feature collection is disabled by
default to keep the sidecar hot path focused on signal detection. This feature
engine reconstructs lightweight depth from the same raw-v1 OBO stream as the
detectors. Snapshot `OBO_ADD` frames update book state but do not count as live
order flow, so reconnect snapshots do not manufacture OFI, volume, or cancel
pressure.

Each snapshot includes raw values and z-scored fields. Z-scores use an online
EWMA baseline per instrument and feature. Defaults are conservative: 10 book
levels, EWMA alpha `0.01`, 30 warmup samples before non-zero z-scores, and an
absolute z-score clamp of 20. Tune with `--feature-depth-levels`,
`--feature-z-alpha`, `--feature-z-min-samples`, and `--feature-z-clip`.

Feature definitions:

- `depth3_imb_z`: z-score of `(top3_bid_qty - top3_ask_qty) /
  (top3_bid_qty + top3_ask_qty)`.
- `weighted_book_imb_z`: same shape as depth imbalance, but levels closer to
  touch get larger weight.
- `imbalance_l3_z`: z-score of the level-3-only bid/ask quantity imbalance.
- `ask_sz_l1_z`: z-score of visible best-ask quantity.
- `touch_depth_ratio_bid_z`: z-score of best-bid quantity divided by total
  sampled bid depth.
- `cancel_touch_bid_qty_15s_z`: z-score of live bid-touch cancel/reduce quantity
  over 15 seconds.
- `ofi_z`: z-score of 15-second order-flow imbalance. Bid additions and ask
  removals are positive; ask additions and bid removals are negative.
- `trade_cnt_imb_300s_z`: z-score of 300-second aggressive buy/sell trade count
  imbalance.
- `trade_touch_buy_qty_15s_z`: z-score of aggressive buy quantity executing at
  the best ask over 15 seconds.
- `trade_vol_120s_z` and `trade_vol_300s_z`: z-scores of traded quantity over
  the named windows.
- `mom_15s_z` and `mom_60s_z`: z-scores of mid-price change over the named
  windows.
- `slope_z`: z-score of near-touch liquidity density across sampled levels.
- `dow_sin`: day-of-week sine from wall clock time. It is context, not z-scored.

Use `--record-frames /path/to/absorption.frames` to write a replayable raw-v1
frame recording. The file format is repeated little-endian `u32` frame length
followed by the raw-v1 frame bytes.

Replay Validation
-----------------

Validate absorption on a sidecar recording deterministically:

```bash
cargo run --release --bin absorption_replay -- /path/to/absorption.frames
```

Validate iceberg candidates on the same recording:

```bash
cargo run --release --bin iceberg_replay -- /path/to/absorption.frames
```

Validate liquidity pulls on the same recording:

```bash
cargo run --release --bin liquidity_pull_replay -- /path/to/absorption.frames
```

Run all participant detectors together and emit combined session diagnostics:

```bash
cargo run --release --bin participant_replay -- /path/to/absorption.frames
```

Replay runs the same frame sequence through each detector twice, dedupes live
frames the same way as the sidecar, and reports the first pass, second pass, and
whether the signal counts/hash are deterministic. It exits non-zero if replay is
not deterministic or if any raw-v1 frame parse errors are present.
