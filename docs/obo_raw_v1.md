## OBO Raw v1 Wire Format and API

### Overview
- Transport: WebSocket (binary). HTTP/3 bytestream planned with identical frames.
- Clients connect to two endpoints per POP (A/B) and keep the first‑arriving frame per `(instrument_id, sequence)`.

### Frame
Each frame is `FrameHeaderV1` (little‑endian) followed by a typed payload.

Header (40 bytes):
```
magic         [4]  = "OBv1"
version       u8   = 1
codec         u8   = 0  (raw structs)
message_type  u16  (see below)
channel_id    u32  = 0  (OBO L3)
instrument_id u64  (venue-defined; here instr as u64)
sequence      u64  (per-instrument monotonic)
send_time_ns  u64  (monotonic)
payload_len   u32
```

Message types:
- 1 HEARTBEAT
- 2 GAP
- 3 SNAPSHOT_START
- 4 SNAPSHOT_END
- 5 SEQ_RESET
- 100 OBO_ADD
- 101 OBO_MODIFY
- 102 OBO_CANCEL
- 103 OBO_EXECUTE
- 104 SNAPSHOT_HDR

OBO payloads are fixed `#[repr(C)]` structs (`OboAddV1`, `OboModifyV1`, `OboCancelV1`, `OboExecuteV1`).

### WebSocket API
`GET /ws?channel=obo&symbols=ESZ5,SPY&codec=raw-v1&from_seq=0&snapshot=1`

Query params:
- `from_seq`: optional global cursor for bus replay (best‑effort). If omitted, tail.
- `snapshot=1`: send full book snapshot before live.

Subprotocol (optional): `Sec-WebSocket-Protocol: obo.raw.v1`.

Authentication (optional): `Authorization: Bearer <token>` if configured.

### Client Dedupe Rule
Keep the first frame for a given `(instrument_id, sequence)`; drop later duplicates from the other endpoint.

### Snapshot Semantics
- `SNAPSHOT_START`, then per‑instrument `SNAPSHOT_HDR` and OBO_ADD for each live order, then `SNAPSHOT_END`.
- Snapshot frames carry `sequence=0`.

### Metrics
- `ws_clients`, `out_frames_total`, `out_bytes_total`, `dropped_clients_total`.


