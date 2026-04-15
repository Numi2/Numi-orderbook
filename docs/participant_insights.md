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

Next signals should not be added until absorption has been validated on recorded
venue sessions and the UI renders the evidence fields clearly.

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
absorption detector, prints each signal as one JSON line, and serves:

- `GET /healthz`: process liveness.
- `GET /ready`: ready after at least one upstream frame has been received.
- `GET /stats`: counters for frames, parsed events, duplicates, parse errors,
  connection attempts, and emitted signals.
- `GET /signals`: recent retained absorption signals.

Use `--record-frames /path/to/absorption.frames` to write a replayable raw-v1
frame recording. The file format is repeated little-endian `u32` frame length
followed by the raw-v1 frame bytes.

Replay Validation
-----------------

Validate a sidecar recording deterministically:

```bash
cargo run --release --bin absorption_replay -- /path/to/absorption.frames
```

Replay runs the same frame sequence through the detector twice, dedupes live
frames the same way as the sidecar, and reports the first pass, second pass, and
whether the signal counts/hash are deterministic. It exits non-zero if replay is
not deterministic or if any raw-v1 frame parse errors are present.
