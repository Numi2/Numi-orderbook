## OBO Raw v1 Wire Format and API

### Overview
- Transport: WebSocket (binary).
- Clients connect to two endpoints per POP (A/B) and keep the first‑arriving frame per `(instrument_id, sequence)`.

### Frame
Each frame is `FrameHeaderV1` (little-endian) followed by a typed payload.

Header (48 bytes):
```
magic           [4]  = "OBv1"
version         u8   = 1
codec           u8   = 0  (raw structs)
message_type    u16  (see below)
channel_id      u32  = 0  (OBO L3)
instrument_id   u64  (venue-defined; here instr as u64)
sequence        u64  (per-instrument monotonic for live OBO events)
global_sequence u64  (bus replay cursor for live OBO events)
send_time_ns    u64  (monotonic)
payload_len     u32
```

Message types:
- 1 HEARTBEAT
- 2 GAP
- 3 SNAPSHOT_START
- 4 SNAPSHOT_END
- 100 OBO_ADD
- 101 OBO_MODIFY
- 102 OBO_CANCEL
- 103 OBO_EXECUTE
- 104 SNAPSHOT_HDR

OBO payloads are fixed `#[repr(C)]` structs (`OboAddV1`, `OboModifyV1`, `OboCancelV1`, `OboExecuteV1`).

Control payloads:
- `GAP`: `GapV1 { from_inclusive: u64, to_inclusive: u64 }`, using global bus
  cursor values. After receiving `GAP`, reconnect with a fresh snapshot or a
  still-retained `from_seq`; do not interpret the range as per-instrument
  sequence numbers.

### WebSocket API
`GET /ws?channel=obo&codec=raw-v1&snapshot=1`

Query params:
- `channel=obo`: optional; rejected if any other value is supplied.
- `codec=raw-v1`: optional; rejected if any other value is supplied.
- `from_seq`: optional global cursor for bus replay. Clients should reconnect
  with the last processed live event `global_sequence + 1`. If omitted, tail.
- `snapshot=1`: send full book snapshot before live. Without `from_seq`, the
  server loads a snapshot that contains its matching replay cursor, sends the
  image, then streams live frames from that cursor. If snapshot loading is
  unavailable, lacks a replay cursor, is older than retained live replay, or
  fails, the connection is rejected instead of silently downgrading to live-only
  delivery.
- `snapshot=1` and `from_seq` cannot be combined.
- Symbol filtering is not supported by raw-v1; `symbols` is rejected instead of
  being accepted silently.
- Unknown query parameters are rejected.
- Duplicate query parameters are rejected.

Subprotocol (optional): `Sec-WebSocket-Protocol: obo.raw.v1`.

Authentication (optional): `Authorization: Bearer <token>` if configured.
WebSocket handshakes and writes are bounded by configurable per-client timeouts.
When no live frame is available before `feeds.obo.client_heartbeat_interval_ms`,
the server sends a `HEARTBEAT` control frame.
Each A/B endpoint pair enforces `feeds.obo.client_max_connections` across
handshakes and established sessions.

### Client Dedupe Rule
For live OBO event frames only, keep the first frame for a given
`(instrument_id, sequence)` and drop later duplicates from the other endpoint.
Do not apply live dedupe to control frames or snapshot payloads.

### Snapshot Semantics
- `SNAPSHOT_START`, then per‑instrument `SNAPSHOT_HDR` and OBO_ADD for each live order, then `SNAPSHOT_END`.
- Snapshot and control frames carry `sequence=0` and `global_sequence=0`.
- Snapshot files written by the server include the global replay cursor that
  immediately follows the image. Snapshot-on-connect uses that cursor as the
  first live frame to stream after `SNAPSHOT_END`.
- If that cursor is no longer retained by the live bus, the server rejects the
  snapshot request before sending the image.

### Metrics
- `ws_clients`, `out_frames_total`, `out_bytes_total`, `dropped_clients_total`.
  `ws_clients` counts established authenticated sessions only.
  `out_frames_total` and `out_bytes_total` count all successfully written
  WebSocket binary frames, including snapshot, live, gap, and heartbeat frames.
  `dropped_clients_total` covers handshake failures, authorization failures,
  replay gaps, and write failures, including slow clients that exceed the
  configured WebSocket write timeout.
