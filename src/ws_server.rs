use anyhow::Context;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use tungstenite::accept_hdr;
use tungstenite::handshake::server::{Request, Response};
use tungstenite::{Message, WebSocket};
use url::Url;

use crate::codec_raw::channel_id;
use crate::codec_raw::msg_type;
use crate::codec_raw::{self, FrameHeaderV1, GapV1};
use crate::metrics;
use crate::pubsub::{Bus, RecvError, Subscription};
use zerocopy::AsBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientQuery {
    from_seq: Option<u64>,
    snapshot: bool,
}

fn parse_query(uri: &str) -> anyhow::Result<ClientQuery> {
    let url = Url::parse(&format!("http://localhost{}", uri))?;
    let mut from_seq: Option<u64> = None;
    let mut snapshot = false;
    let mut seen_snapshot = false;
    let mut seen_channel = false;
    let mut seen_codec = false;
    for (k, v) in url.query_pairs() {
        match &*k {
            "from_seq" => {
                if from_seq.is_some() {
                    anyhow::bail!("duplicate from_seq query parameter");
                }
                from_seq = Some(
                    v.parse::<u64>()
                        .with_context(|| format!("invalid from_seq query value: {v}"))?,
                );
            }
            "snapshot" => {
                if seen_snapshot {
                    anyhow::bail!("duplicate snapshot query parameter");
                }
                seen_snapshot = true;
                match &*v {
                    "1" | "true" => snapshot = true,
                    "0" | "false" => snapshot = false,
                    _ => anyhow::bail!("invalid snapshot query value: {v}"),
                }
            }
            "channel" => {
                if seen_channel {
                    anyhow::bail!("duplicate channel query parameter");
                }
                seen_channel = true;
                if v != "obo" {
                    anyhow::bail!("unsupported channel query value: {v}");
                }
            }
            "codec" => {
                if seen_codec {
                    anyhow::bail!("duplicate codec query parameter");
                }
                seen_codec = true;
                if v != "raw-v1" {
                    anyhow::bail!("unsupported codec query value: {v}");
                }
            }
            "symbols" => {
                anyhow::bail!("symbol filtering is not supported by the raw-v1 feed");
            }
            _ => {
                anyhow::bail!("unsupported query parameter: {k}");
            }
        }
    }
    if snapshot && from_seq.is_some() {
        anyhow::bail!("snapshot and from_seq cannot be combined");
    }
    Ok(ClientQuery { from_seq, snapshot })
}

pub fn spawn_pair(
    bus: Bus,
    addr_a: String,
    addr_b: String,
    snapshot_path: Option<String>,
    auth_token: Option<String>,
    client_write_timeout_ms: u64,
    client_handshake_timeout_ms: u64,
    client_heartbeat_interval_ms: u64,
    client_max_connections: usize,
    client_nodelay: bool,
) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let limiter = ClientLimiter::new(client_max_connections);
    let b1 = bus.clone();
    let a1 = addr_a.clone();
    let snap1 = snapshot_path.clone();
    let tok1 = auth_token.clone();
    let limiter1 = limiter.clone();
    let t1 = thread::Builder::new()
        .name("ws-A".into())
        .spawn(move || {
            run_ws_listener(
                &b1,
                &a1,
                snap1.as_deref(),
                tok1.as_deref(),
                client_write_timeout_ms,
                client_handshake_timeout_ms,
                client_heartbeat_interval_ms,
                limiter1,
                client_nodelay,
            );
        })
        .expect("spawn ws A");

    let b2 = bus;
    let a2 = addr_b.clone();
    let snap2 = snapshot_path;
    let tok2 = auth_token;
    let limiter2 = limiter;
    let t2 = thread::Builder::new()
        .name("ws-B".into())
        .spawn(move || {
            run_ws_listener(
                &b2,
                &a2,
                snap2.as_deref(),
                tok2.as_deref(),
                client_write_timeout_ms,
                client_handshake_timeout_ms,
                client_heartbeat_interval_ms,
                limiter2,
                client_nodelay,
            );
        })
        .expect("spawn ws B");

    (t1, t2)
}

fn run_ws_listener(
    bus: &Bus,
    addr: &str,
    snapshot_path: Option<&str>,
    auth_token: Option<&str>,
    client_write_timeout_ms: u64,
    client_handshake_timeout_ms: u64,
    client_heartbeat_interval_ms: u64,
    limiter: ClientLimiter,
    client_nodelay: bool,
) {
    let listener = TcpListener::bind(addr).expect("bind ws");
    log::info!("ws listening on {}", addr);
    for stream in listener.incoming().flatten() {
        let Some(permit) = limiter.try_acquire() else {
            metrics::inc_dropped_clients();
            continue;
        };
        let b = bus.clone();
        let snap = snapshot_path.map(|s| s.to_string());
        let tok = auth_token.map(|s| s.to_string());
        thread::spawn(move || {
            let _permit = permit;
            let r = handle_client(
                b,
                stream,
                snap,
                tok,
                client_write_timeout_ms,
                client_handshake_timeout_ms,
                client_heartbeat_interval_ms,
                client_nodelay,
            );
            if let Err(e) = r {
                log::warn!("ws client error: {:?}", e);
            }
        });
    }
}

fn handle_client(
    bus: Bus,
    stream: TcpStream,
    snapshot_path: Option<String>,
    auth_token: Option<String>,
    client_write_timeout_ms: u64,
    client_handshake_timeout_ms: u64,
    client_heartbeat_interval_ms: u64,
    client_nodelay: bool,
) -> anyhow::Result<()> {
    if client_nodelay {
        stream.set_nodelay(true)?;
    }
    stream.set_read_timeout(Some(Duration::from_millis(client_handshake_timeout_ms)))?;
    stream.set_write_timeout(Some(Duration::from_millis(client_write_timeout_ms)))?;

    let req_uri = Arc::new(Mutex::new(String::new()));
    let auth_header: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let req_uri_clone = req_uri.clone();
    let auth_header_clone = auth_header.clone();
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, resp: Response| {
        *req_uri_clone.lock().unwrap() = req.uri().to_string();
        if let Some(hv) = req.headers().get("Authorization") {
            if let Ok(s) = hv.to_str() {
                *auth_header_clone.lock().unwrap() = Some(s.to_string());
            }
        }
        Ok(resp)
    };
    let mut ws: WebSocket<TcpStream> = match accept_hdr(stream, callback) {
        Ok(ws) => ws,
        Err(err) => {
            metrics::inc_dropped_clients();
            return Err(err.into());
        }
    };
    ws.get_mut().set_read_timeout(None)?;

    if let Some(token) = auth_token {
        let ok = auth_header
            .lock()
            .unwrap()
            .as_deref()
            .map(|v| v == format!("Bearer {}", token))
            .unwrap_or(false);
        if !ok {
            let _ = ws.close(None);
            metrics::inc_dropped_clients();
            anyhow::bail!("unauthorized");
        }
    }

    let ClientQuery { from_seq, snapshot } =
        parse_query(&req_uri.lock().unwrap()).map_err(|err| {
            metrics::inc_dropped_clients();
            err
        })?;
    let _client_gauge = ConnectedClientGauge::new();
    let heartbeat_interval = Duration::from_millis(client_heartbeat_interval_ms);
    let mut sub: Subscription = bus.subscribe();
    let mut frames_since_lag_sample: u32 = 0;
    if let Some(g) = from_seq {
        sub.set_cursor(g);
    } else if !snapshot {
        sub.set_cursor_to_tail();
    }

    if snapshot {
        let Some(path) = snapshot_path else {
            metrics::inc_dropped_clients();
            anyhow::bail!("snapshot requested but snapshot path is not configured");
        };
        let loaded = crate::snapshot::load_image(std::path::Path::new(&path))
            .with_context(|| format!("load snapshot {path}"))
            .map_err(|err| {
                metrics::inc_dropped_clients();
                err
            })?;
        let Some(replay_from) = loaded.replay_from else {
            metrics::inc_dropped_clients();
            anyhow::bail!("snapshot {path} does not contain a replay cursor");
        };
        sub.set_cursor(replay_from);
        if !sub.cursor_available() {
            metrics::inc_dropped_clients();
            anyhow::bail!(
                "snapshot {path} replay cursor {replay_from} is outside retained live range"
            );
        }
        let export = loaded.book.export();
        send_control(&mut ws, msg_type::SNAPSHOT_START, &[])?;
        for ie in export.instruments {
            let hdr = crate::codec_raw::FullBookSnapshotHdrV1 {
                level_count: 0,
                total_orders: ie.orders.len() as u32,
            };
            send_control(&mut ws, msg_type::SNAPSHOT_HDR, hdr.as_bytes())?;
            for o in ie.orders {
                let side = match o.side {
                    crate::parser::Side::Bid => 0,
                    crate::parser::Side::Ask => 1,
                };
                let add = crate::codec_raw::OboAddV1 {
                    order_id: o.order_id,
                    price_e8: o.price,
                    qty: o.qty as u64,
                    side,
                    flags: 0,
                };
                let frame = build_frame(msg_type::OBO_ADD, add.as_bytes(), ie.instr as u64, 0);
                send_binary(&mut ws, frame)?;
            }
        }
        send_control(&mut ws, msg_type::SNAPSHOT_END, &[])?;
    }

    loop {
        match sub.recv_next_timeout(heartbeat_interval) {
            Ok(Some(bytes)) => {
                send_binary(&mut ws, bytes.to_vec())?;
                frames_since_lag_sample = frames_since_lag_sample.wrapping_add(1);
                if (frames_since_lag_sample & 0x1ff) == 0 {
                    metrics::set_queue_len("pubsub_lag", sub.lag() as usize);
                }
            }
            Ok(None) => {
                send_control(&mut ws, msg_type::HEARTBEAT, &[])?;
            }
            Err(RecvError::Gap { from, to }) => {
                // send GAP control and terminate
                let gap = GapV1 {
                    from_inclusive: from,
                    to_inclusive: to,
                };
                send_control(&mut ws, msg_type::GAP, gap.as_bytes())?;
                metrics::inc_dropped_clients();
                let _ = ws.close(None);
                break;
            }
        }
    }
    Ok(())
}

struct ConnectedClientGauge;

impl ConnectedClientGauge {
    fn new() -> Self {
        metrics::inc_ws_clients(1);
        Self
    }
}

impl Drop for ConnectedClientGauge {
    fn drop(&mut self) {
        metrics::inc_ws_clients(-1);
    }
}

#[derive(Clone)]
struct ClientLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

struct ClientPermit {
    active: Arc<AtomicUsize>,
}

impl ClientLimiter {
    fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    fn try_acquire(&self) -> Option<ClientPermit> {
        let mut current = self.active.load(Ordering::Relaxed);
        loop {
            if current >= self.max {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(ClientPermit {
                        active: self.active.clone(),
                    });
                }
                Err(next) => current = next,
            }
        }
    }
}

impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Release);
    }
}

fn send_control(ws: &mut WebSocket<TcpStream>, ty: u16, payload: &[u8]) -> anyhow::Result<()> {
    let frame = build_frame(ty, payload, 0, 0);
    send_binary(ws, frame)?;
    Ok(())
}

fn send_binary(ws: &mut WebSocket<TcpStream>, frame: Vec<u8>) -> anyhow::Result<()> {
    let frame_len = frame.len();
    if let Err(err) = ws.send(Message::Binary(frame)) {
        metrics::inc_dropped_clients();
        return Err(err.into());
    }
    metrics::inc_out_frames();
    metrics::inc_out_bytes(frame_len);
    Ok(())
}

fn build_frame(msg_ty: u16, payload: &[u8], instrument_id: u64, sequence: u64) -> Vec<u8> {
    let hdr = FrameHeaderV1 {
        magic: codec_raw::MAGIC,
        version: codec_raw::VERSION_V1,
        codec: codec_raw::codec::RAW_V1,
        message_type: msg_ty,
        channel_id: channel_id::OBO_L3,
        instrument_id,
        sequence,
        global_sequence: 0,
        send_time_ns: crate::util::now_nanos(),
        payload_len: payload.len() as u32,
    };
    let mut v = Vec::with_capacity(std::mem::size_of::<FrameHeaderV1>() + payload.len());
    v.extend_from_slice(hdr.as_bytes());
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot_and_from_seq_query() {
        assert_eq!(
            parse_query("/ws?channel=obo&codec=raw-v1&snapshot=1").unwrap(),
            ClientQuery {
                from_seq: None,
                snapshot: true
            }
        );
        assert_eq!(
            parse_query("/ws?from_seq=7").unwrap(),
            ClientQuery {
                from_seq: Some(7),
                snapshot: false
            }
        );
    }

    #[test]
    fn invalid_query_values_are_rejected() {
        assert!(parse_query("/ws?from_seq=bad&snapshot=false").is_err());
        assert!(parse_query("/ws?snapshot=maybe").is_err());
        assert!(parse_query("/ws?channel=depth").is_err());
        assert!(parse_query("/ws?codec=json").is_err());
        assert!(parse_query("/ws?symbols=ESZ5").is_err());
        assert!(parse_query("/ws?foo=bar").is_err());
        assert!(parse_query("/ws?from_seq=42&snapshot=1").is_err());
        assert!(parse_query("/ws?from_seq=1&from_seq=2").is_err());
        assert!(parse_query("/ws?snapshot=0&snapshot=1").is_err());
        assert!(parse_query("/ws?channel=obo&channel=obo").is_err());
        assert!(parse_query("/ws?codec=raw-v1&codec=raw-v1").is_err());
    }

    #[test]
    fn empty_query_defaults_to_tail_without_snapshot() {
        assert_eq!(
            parse_query("/ws").unwrap(),
            ClientQuery {
                from_seq: None,
                snapshot: false
            }
        );
    }

    #[test]
    fn client_limiter_caps_and_releases_connections() {
        let limiter = ClientLimiter::new(1);
        let permit = limiter.try_acquire().expect("first client allowed");
        assert!(limiter.try_acquire().is_none());
        drop(permit);
        assert!(limiter.try_acquire().is_some());
    }
}
