use std::net::TcpListener;
use std::thread;
use std::sync::Arc;
use std::sync::Mutex;
use tungstenite::handshake::server::{Request, Response};
use tungstenite::accept_hdr;
use tungstenite::{Message, WebSocket};
use std::net::TcpStream;
use url::Url;

use crate::pubsub::{Bus, Subscription, RecvError};
use crate::codec_raw::{self, FrameHeaderV1, GapV1};
use crate::codec_raw::msg_type;
use crate::codec_raw::channel_id;
use crate::metrics;
use zerocopy::AsBytes;

fn parse_query(uri: &str) -> (Option<u64>, bool) {
    if let Ok(url) = Url::parse(&format!("http://localhost{}", uri)) {
        let mut from_seq: Option<u64> = None;
        let mut snapshot = false;
        for (k, v) in url.query_pairs() {
            match &*k {
                "from_seq" => { if let Ok(n) = v.parse::<u64>() { from_seq = Some(n); } },
                "snapshot" => { snapshot = v == "1" || v == "true"; },
                _ => {}
            }
        }
        return (from_seq, snapshot);
    }
    (None, false)
}

pub fn spawn_pair(bus: Bus, addr_a: String, addr_b: String, snapshot_path: Option<String>, auth_token: Option<String>) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let b1 = bus.clone();
    let a1 = addr_a.clone();
    let snap1 = snapshot_path.clone();
    let tok1 = auth_token.clone();
    let t1 = thread::Builder::new().name("ws-A".into()).spawn(move || {
        run_ws_listener(&b1, &a1, snap1.as_deref(), tok1.as_deref());
    }).expect("spawn ws A");

    let b2 = bus;
    let a2 = addr_b.clone();
    let snap2 = snapshot_path;
    let tok2 = auth_token;
    let t2 = thread::Builder::new().name("ws-B".into()).spawn(move || {
        run_ws_listener(&b2, &a2, snap2.as_deref(), tok2.as_deref());
    }).expect("spawn ws B");

    (t1, t2)
}

fn run_ws_listener(bus: &Bus, addr: &str, snapshot_path: Option<&str>, auth_token: Option<&str>) {
    let listener = TcpListener::bind(addr).expect("bind ws");
    log::info!("ws listening on {}", addr);
    for stream in listener.incoming().flatten() {
        let b = bus.clone();
        let snap = snapshot_path.map(|s| s.to_string());
        let tok = auth_token.map(|s| s.to_string());
        thread::spawn(move || {
            metrics::inc_ws_clients(1);
            let r = handle_client(b, stream, snap, tok);
            metrics::inc_ws_clients(-1);
            if let Err(e) = r {
                log::warn!("ws client error: {:?}", e);
            }
        });
    }
}

fn handle_client(bus: Bus, stream: TcpStream, snapshot_path: Option<String>, auth_token: Option<String>) -> anyhow::Result<()> {
    let req_uri = Arc::new(Mutex::new(String::new()));
    let auth_header: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let req_uri_clone = req_uri.clone();
    let auth_header_clone = auth_header.clone();
    let callback = move |req: &Request, resp: Response| {
        *req_uri_clone.lock().unwrap() = req.uri().to_string();
        if let Some(hv) = req.headers().get("Authorization") {
            if let Ok(s) = hv.to_str() {
                *auth_header_clone.lock().unwrap() = Some(s.to_string());
            }
        }
        Ok(resp)
    };
    let mut ws: WebSocket<TcpStream> = accept_hdr(stream, callback)?;

    if let Some(token) = auth_token {
        let ok = auth_header.lock().unwrap().as_deref().map(|v| v == format!("Bearer {}", token)).unwrap_or(false);
        if !ok {
            let _ = ws.close(None);
            anyhow::bail!("unauthorized");
        }
    }

    let (from_seq, snapshot) = parse_query(&req_uri.lock().unwrap());
    if snapshot {
        if let Some(path) = snapshot_path {
            if let Ok(book) = crate::snapshot::load(std::path::Path::new(&path)) {
                let export = book.export();
                // SNAPSHOT_START
                send_control(&mut ws, msg_type::SNAPSHOT_START, &[])?;
                for ie in export.instruments {
                    let hdr = crate::codec_raw::FullBookSnapshotHdrV1 { level_count: 0, total_orders: ie.orders.len() as u32 };
                    send_control(&mut ws, msg_type::SNAPSHOT_HDR, hdr.as_bytes())?;
                    for o in ie.orders {
                        let side = match o.side { crate::parser::Side::Bid => 0, crate::parser::Side::Ask => 1 };
                        let add = crate::codec_raw::OboAddV1 { order_id: o.order_id, price_e8: o.price, qty: o.qty as u64, side, flags: 0 };
                        let frame = build_frame(msg_type::OBO_ADD, add.as_bytes(), ie.instr as u64, 0);
                        ws.send(Message::Binary(frame))?;
                    }
                }
                // SNAPSHOT_END
                send_control(&mut ws, msg_type::SNAPSHOT_END, &[])?;
            }
        }
    }

    let mut sub: Subscription = bus.subscribe();
    if let Some(g) = from_seq { sub.set_cursor(g); } else { sub.set_cursor_to_tail(); }

    loop {
        match sub.recv_next_blocking() {
            Ok(bytes) => {
                metrics::inc_out_frames();
                metrics::inc_out_bytes(bytes.len());
                ws.send(Message::Binary(bytes.to_vec()))?;
            }
            Err(RecvError::Gap { from, to }) => {
                // send GAP control and terminate
                let gap = GapV1 { from_inclusive: from, to_inclusive: to };
                send_control(&mut ws, msg_type::GAP, gap.as_bytes())?;
                metrics::inc_dropped_clients();
                let _ = ws.close(None);
                break;
            }
        }
    }
    Ok(())
}

fn send_control(ws: &mut WebSocket<TcpStream>, ty: u16, payload: &[u8]) -> anyhow::Result<()> {
    let frame = build_frame(ty, payload, 0, 0);
    ws.send(Message::Binary(frame))?;
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
        send_time_ns: crate::util::now_nanos(),
        payload_len: payload.len() as u32,
    };
    let mut v = Vec::with_capacity(std::mem::size_of::<FrameHeaderV1>() + payload.len());
    v.extend_from_slice(hdr.as_bytes());
    v.extend_from_slice(payload);
    v
}


