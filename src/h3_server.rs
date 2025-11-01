use std::net::ToSocketAddrs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use bytes::Bytes;
use h3::server::{self, RequestStream};
use h3::quic::BidiStream;
use h3_quinn::Connection;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::{Certificate, PrivateKey};

use crate::pubsub::{Bus, Subscription, RecvError};

pub fn spawn_pair(bus: Bus, addr_a: String, addr_b: String, tls_cert: Option<String>, tls_key: Option<String>) -> (thread::JoinHandle<()>, thread::JoinHandle<()>) {
    let t1 = {
        let b = bus.clone(); let c = tls_cert.clone(); let k = tls_key.clone(); let a = addr_a.clone();
        thread::Builder::new().name("h3-A".into()).spawn(move || {
            run_h3_listener(&b, &a, c.as_deref(), k.as_deref());
        }).expect("spawn h3 A")
    };
    let t2 = {
        let b = bus; let c = tls_cert; let k = tls_key; let a = addr_b.clone();
        thread::Builder::new().name("h3-B".into()).spawn(move || {
            run_h3_listener(&b, &a, c.as_deref(), k.as_deref());
        }).expect("spawn h3 B")
    };
    (t1, t2)
}

fn run_h3_listener(bus: &Bus, bind_addr: &str, cert_path: Option<&str>, key_path: Option<&str>) {
    let (certs, key) = load_or_gen(cert_path, key_path);
    let server_cfg = make_server_config(certs, key);
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(3)));
    let server_cfg = ServerConfig::with_crypto(Arc::new(server_cfg));
    let addr = bind_addr.to_socket_addrs().ok().and_then(|mut it| it.next()).expect("h3 bind address");
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
                            let _ = resp.send_response(h3::Headers::new(vec![":status", "200"].iter().map(|s| (*s).into()).collect()));
                            let mut send = resp.send_data();
                            let mut sub = busc.subscribe();
                            // No from_seq parsing here for brevity; start at tail
                            sub.set_cursor_to_tail();
                            loop {
                                match sub.recv_next_blocking() {
                                    Ok(bytes) => { let _ = send.write(&bytes); }
                                    Err(RecvError::Gap) => { break; }
                                    Err(RecvError::Closed) => { break; }
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

fn load_or_gen(cert_path: Option<&str>, key_path: Option<&str>) -> (Vec<Certificate>, PrivateKey) {
    if let (Some(c), Some(k)) = (cert_path, key_path) {
        if let (Ok(cb), Ok(kb)) = (std::fs::read(c), std::fs::read(k)) {
            // Try PEM
            if let Ok(mut certs) = rustls_pemfile::certs(&mut &*cb) {
                if let Ok(Some(pk)) = rustls_pemfile::read_one(&mut &*kb) {
                    if let rustls_pemfile::Item::PKCS8Key(key_bytes) = pk { return (certs.into_iter().map(Certificate).collect(), PrivateKey(key_bytes)); }
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
    let mut cfg = rustls::ServerConfig::builder().with_safe_defaults().with_no_client_auth().with_single_cert(certs, key).expect("cert");
    cfg.alpn_protocols = vec![b"h3".to_vec()];
    cfg
}


