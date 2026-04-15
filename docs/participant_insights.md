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

Next signals should not be added until absorption has a sidecar/API integration,
replay validation, and user-facing evidence rendering.
