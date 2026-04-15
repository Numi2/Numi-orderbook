use hashbrown::HashMap;
use std::env;
use std::thread;
use tungstenite::Message;

const FRAME_HEADER_LEN: usize = 48;
const MAGIC: &[u8; 4] = b"OBv1";
const MSG_HEARTBEAT: u16 = 1;
const MSG_GAP: u16 = 2;
const MSG_SNAPSHOT_START: u16 = 3;
const MSG_SNAPSHOT_END: u16 = 4;
const MSG_OBO_ADD: u16 = 100;
const MSG_OBO_MODIFY: u16 = 101;
const MSG_OBO_CANCEL: u16 = 102;
const MSG_OBO_EXECUTE: u16 = 103;
const MSG_SNAPSHOT_HDR: u16 = 104;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} ws_url_a ws_url_b [auth_token]", args[0]);
        std::process::exit(1);
    }
    let url_a = args[1].clone();
    let url_b = args[2].clone();
    let auth = if args.len() > 3 {
        Some(args[3].clone())
    } else {
        None
    };

    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    let txa = tx.clone();
    let auth_a = auth.clone();
    thread::spawn(move || connect_and_forward(&url_a, auth_a.as_deref(), txa));
    let txb = tx.clone();
    let auth_b = auth.clone();
    thread::spawn(move || connect_and_forward(&url_b, auth_b.as_deref(), txb));

    let mut last_seq_by_instr: HashMap<u64, u64> = HashMap::new();
    let mut in_snapshot = false;
    loop {
        if let Ok(frame) = rx.recv() {
            let Some(header) = parse_header(&frame) else {
                continue;
            };

            match header.message_type {
                MSG_HEARTBEAT => {
                    println!("heartbeat");
                    continue;
                }
                MSG_GAP => {
                    println!("gap frame received");
                    continue;
                }
                MSG_SNAPSHOT_START => {
                    in_snapshot = true;
                    println!("snapshot_start");
                    continue;
                }
                MSG_SNAPSHOT_END => {
                    in_snapshot = false;
                    println!("snapshot_end");
                    continue;
                }
                MSG_SNAPSHOT_HDR => {
                    println!("snapshot_hdr instr={}", header.instrument_id);
                    continue;
                }
                MSG_OBO_ADD | MSG_OBO_MODIFY | MSG_OBO_CANCEL | MSG_OBO_EXECUTE => {}
                _ => {
                    println!(
                        "unknown frame type={} instr={} seq={} global={}",
                        header.message_type,
                        header.instrument_id,
                        header.sequence,
                        header.global_sequence
                    );
                    continue;
                }
            }

            if !in_snapshot {
                let e = last_seq_by_instr.entry(header.instrument_id).or_insert(0);
                if header.sequence <= *e {
                    continue;
                }
                *e = header.sequence;
            }
            println!(
                "instr={} seq={} global={} type={} snapshot={}",
                header.instrument_id,
                header.sequence,
                header.global_sequence,
                header.message_type,
                in_snapshot
            );
        }
    }
}

fn connect_and_forward(url: &str, auth: Option<&str>, tx: crossbeam_channel::Sender<Vec<u8>>) {
    let mut req = tungstenite::http::Request::builder().uri(url);
    if let Some(tok) = auth {
        req = req.header("Authorization", format!("Bearer {}", tok));
    }
    let req = req.body(()).unwrap();
    let (mut ws, _) = tungstenite::connect(req).expect("ws connect");
    while let Ok(msg) = ws.read() {
        if let Message::Binary(b) = msg {
            let _ = tx.send(b);
        }
    }
}

#[inline]
fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
#[inline]
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
#[inline]
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

struct Header {
    message_type: u16,
    instrument_id: u64,
    sequence: u64,
    global_sequence: u64,
}

fn parse_header(frame: &[u8]) -> Option<Header> {
    if frame.len() < FRAME_HEADER_LEN || &frame[0..4] != MAGIC {
        return None;
    }
    let payload_len = le_u32(&frame[44..48]) as usize;
    if frame.len() != FRAME_HEADER_LEN + payload_len {
        return None;
    }
    Some(Header {
        message_type: le_u16(&frame[6..8]),
        instrument_id: le_u64(&frame[12..20]),
        sequence: le_u64(&frame[20..28]),
        global_sequence: le_u64(&frame[28..36]),
    })
}
