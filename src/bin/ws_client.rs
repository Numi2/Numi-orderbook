use hashbrown::HashMap;
use std::env;
use std::thread;
use tungstenite::Message;

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
    loop {
        if let Ok(frame) = rx.recv() {
            if frame.len() < 40 {
                continue;
            }
            let instr = le_u64(&frame[16..24]);
            let seq = le_u64(&frame[24..32]);
            let mtype = le_u16(&frame[6..8]);
            let e = last_seq_by_instr.entry(instr).or_insert(0);
            if seq <= *e {
                continue;
            } // drop duplicate/old
            *e = seq;
            println!("instr={} seq={} type={}", instr, seq, mtype);
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
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}
