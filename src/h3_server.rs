use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use bytes::Bytes;
use h3::quic::BidiStream;
use h3::server::{self, RequestStream};
use h3_quinn::Connection;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::{Certificate, PrivateKey};

use crate::codec_raw::{self, FrameHeaderV1};
use crate::codec_raw::{channel_id, msg_type};
use crate::pubsub::{Bus, RecvError, Subscription};
use url::Url;
use zerocopy::AsBytes;

pub fn spawn_pair(
    bus: Bus,
    addr_a: String,
    addr_b: String,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    snapshot_path: Option<String>,
) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let t1 = {
        let b = bus.clone();
        let c = tls_cert.clone();
        let k = tls_key.clone();
        let a = addr_a.clone();
        let s = snapshot_path.clone();
        thread::Builder::new()
            .name("h3-A".into())
            .spawn(move || {
                run_h3_listener(&b, &a, c.as_deref(), k.as_deref(), s.as_deref());
            })
            .expect("spawn h3 A")
    };
    let t2 = {
        let b = bus;
        let c = tls_cert;
        let k = tls_key;
        let a = addr_b.clone();
        let s = snapshot_path;
        thread::Builder::new()
            .name("h3-B".into())
            .spawn(move || {
                run_h3_listener(&b, &a, c.as_deref(), k.as_deref(), s.as_deref());
            })
            .expect("spawn h3 B")
    };
    (t1, t2)
}

fn run_h3_listener(
    bus: &Bus,
    bind_addr: &str,
    cert_path: Option<&str>,
    key_path: Option<&str>,
    snapshot_path: Option<&str>,
) {
    let (certs, key) = load_or_gen(cert_path, key_path);
    let server_cfg = make_server_config(certs, key);
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(3)));
    let server_cfg = ServerConfig::with_crypto(Arc::new(server_cfg));
    let addr = bind_addr
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
        .expect("h3 bind address");
    let (endpoint, mut incoming) = Endpoint::server(server_cfg, addr).expect("quinn server");
    log::info!("h3 listening on {}", bind_addr);

    while let Some(conn) = incoming.next() {
        let busc = bus.clone();
        thread::spawn(move || {
            if let Ok(new_conn) = conn.await {
                let quinn_conn = new_conn.connection;
                let mut h3 = server::Connection::new(Connection::new(quinn_conn)).expect("h3 conn");
                loop {
                    match h3.accept().ok() {
                        Some((req, resp)) => {
                            // Basic routing: only /h3/obo
                            let path = req.uri().path().to_string();
                            let query = req.uri().query().unwrap_or("").to_string();
                            let _ = resp.send_response(h3::Headers::new(
                                vec![":status", "200"].iter().map(|s| (*s).into()).collect(),
                            ));
                            let mut send = resp.send_data();
                            let mut sub = busc.subscribe();
                            // Parse query params: from_seq=..., snapshot=1
                            let (from_seq, snapshot) = parse_query_params(&query);
                            if let Some(g) = from_seq {
                                sub.set_cursor(g);
                            } else {
                                sub.set_cursor_to_tail();
                            }

                            // Optional: send chunked snapshot first
                            if snapshot {
                                if let Some(path) = snapshot_path {
                                    if let Ok(book) =
                                        crate::snapshot::load(std::path::Path::new(path))
                                    {
                                        let export = book.export();
                                        // SNAPSHOT_START
                                        let _ = send.write(&build_frame(
                                            msg_type::SNAPSHOT_START,
                                            &[],
                                            0,
                                            0,
                                        ));
                                        for ie in export.instruments {
                                            let hdr = crate::codec_raw::FullBookSnapshotHdrV1 {
                                                level_count: 0,
                                                total_orders: ie.orders.len() as u32,
                                            };
                                            let _ = send.write(&build_frame(
                                                msg_type::SNAPSHOT_HDR,
                                                hdr.as_bytes(),
                                                ie.instr as u64,
                                                0,
                                            ));
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
                                                let _ = send.write(&build_frame(
                                                    msg_type::OBO_ADD,
                                                    add.as_bytes(),
                                                    ie.instr as u64,
                                                    0,
                                                ));
                                            }
                                        }
                                        let _ = send.write(&build_frame(
                                            msg_type::SNAPSHOT_END,
                                            &[],
                                            0,
                                            0,
                                        ));
                                    }
                                }
                            }
                            loop {
                                match sub.recv_next_blocking() {
                                    Ok(bytes) => {
                                        let _ = send.write(&bytes);
                                    }
                                    Err(RecvError::Gap { .. }) => {
                                        break;
                                    }
                                }
                            }
                            let _ = send.finish();
                        }
                        None => break,
                    }
                }
            }
        });
    }
    drop(endpoint);
}

fn parse_query_params(qs: &str) -> (Option<u64>, bool) {
    if qs.is_empty() {
        return (None, false);
    }
    let url = format!("http://localhost/?{}", qs);
    if let Ok(u) = Url::parse(&url) {
        let mut from_seq: Option<u64> = None;
        let mut snapshot = false;
        for (k, v) in u.query_pairs() {
            match &*k {
                "from_seq" => {
                    if let Ok(n) = v.parse::<u64>() {
                        from_seq = Some(n);
                    }
                }
                "snapshot" => {
                    snapshot = v == "1" || v == "true";
                }
                _ => {}
            }
        }
        return (from_seq, snapshot);
    }
    (None, false)
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

fn load_or_gen(cert_path: Option<&str>, key_path: Option<&str>) -> (Vec<Certificate>, PrivateKey) {
    if let (Some(c), Some(k)) = (cert_path, key_path) {
        if let (Ok(cb), Ok(kb)) = (std::fs::read(c), std::fs::read(k)) {
            // Try PEM
            if let Ok(mut certs) = rustls_pemfile::certs(&mut &*cb) {
                if let Ok(Some(pk)) = rustls_pemfile::read_one(&mut &*kb) {
                    if let rustls_pemfile::Item::PKCS8Key(key_bytes) = pk {
                        return (
                            certs.into_iter().map(Certificate).collect(),
                            PrivateKey(key_bytes),
                        );
                    }
                }
            }
        }
    }
    // Generate self-signed
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = PrivateKey(cert.serialize_private_key_der());
    let cert = Certificate(cert.serialize_der().unwrap());
    (vec![cert], key)
}

fn make_server_config(certs: Vec<Certificate>, key: PrivateKey) -> rustls::ServerConfig {
    let mut cfg = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("cert");
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    cfg
}
