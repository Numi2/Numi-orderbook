use anyhow::Context;
use crossbeam_channel::{bounded, Sender};
use orderbook::insights::{
    parse_obo_frame, AbsorptionConfig, AbsorptionDetector, AbsorptionSignal, OboLiveDedupe,
};
use orderbook::util::now_nanos;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tungstenite::Message;

const DEFAULT_LISTEN: &str = "127.0.0.1:9201";
const DEFAULT_RETAIN_SIGNALS: usize = 1024;
const FRAME_QUEUE_CAPACITY: usize = 65_536;
const RECORD_FLUSH_INTERVAL: u64 = 1024;
const MAX_RECORDED_FRAME_LEN: usize = u32::MAX as usize;

#[derive(Debug)]
struct Args {
    urls: Vec<String>,
    listen: String,
    auth_token: Option<String>,
    retain_signals: usize,
    record_frames: Option<PathBuf>,
    absorption: AbsorptionConfig,
}

#[derive(Debug)]
struct SidecarState {
    retain_signals: usize,
    started_at_ns: u64,
    frames_received: u64,
    control_frames: u64,
    parsed_events: u64,
    duplicate_events: u64,
    parse_errors: u64,
    signals_emitted: u64,
    connection_attempts: u64,
    websocket_errors: u64,
    record_write_errors: u64,
    last_frame_ns: Option<u64>,
    last_signal_ns: Option<u64>,
    last_error: Option<String>,
    recent_signals: VecDeque<AbsorptionSignal>,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    started_at_ns: u64,
    frames_received: u64,
    control_frames: u64,
    parsed_events: u64,
    duplicate_events: u64,
    parse_errors: u64,
    signals_emitted: u64,
    connection_attempts: u64,
    websocket_errors: u64,
    record_write_errors: u64,
    last_frame_ns: Option<u64>,
    last_signal_ns: Option<u64>,
    recent_signal_count: usize,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SignalsResponse {
    signals: Vec<AbsorptionSignal>,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse()?;
    let state = Arc::new(Mutex::new(SidecarState::new(args.retain_signals)));
    let _api = spawn_http(args.listen.clone(), state.clone())?;
    let (tx, rx) = bounded::<Vec<u8>>(FRAME_QUEUE_CAPACITY);

    for (idx, url) in args.urls.iter().enumerate() {
        let auth = args.auth_token.clone();
        let state_for_thread = state.clone();
        let tx_for_thread = tx.clone();
        let url = url.clone();
        thread::Builder::new()
            .name(format!("absorption-ws-{idx}"))
            .spawn(move || connect_loop(url, auth, tx_for_thread, state_for_thread))?;
    }
    drop(tx);

    let mut detector = AbsorptionDetector::new(args.absorption);
    let mut dedupe = OboLiveDedupe::new();
    let mut recorder = match args.record_frames {
        Some(path) => {
            Some(BufWriter::new(File::create(&path).with_context(|| {
                format!("create frame recording {path:?}")
            })?))
        }
        None => None,
    };
    let mut recorded_since_flush = 0_u64;

    log::info!(
        "absorption sidecar listening on {} and consuming {} feed(s)",
        args.listen,
        args.urls.len()
    );

    while let Ok(frame) = rx.recv() {
        record_frame_if_enabled(
            &mut recorder,
            &mut recorded_since_flush,
            &frame,
            state.as_ref(),
        );
        process_frame(&frame, &mut detector, &mut dedupe, state.as_ref());
    }

    Ok(())
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut parsed = Self {
            urls: Vec::new(),
            listen: DEFAULT_LISTEN.to_string(),
            auth_token: None,
            retain_signals: DEFAULT_RETAIN_SIGNALS,
            record_frames: None,
            absorption: AbsorptionConfig::default(),
        };

        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "-h" | "--help" => {
                    usage();
                    std::process::exit(0);
                }
                "--url" => parsed.urls.push(next_value(&mut args, "--url")?),
                "--listen" => parsed.listen = next_value(&mut args, "--listen")?,
                "--auth-token" => parsed.auth_token = Some(next_value(&mut args, "--auth-token")?),
                "--retain-signals" => {
                    parsed.retain_signals = parse_next(&mut args, "--retain-signals")?
                }
                "--record-frames" => {
                    parsed.record_frames =
                        Some(PathBuf::from(next_value(&mut args, "--record-frames")?))
                }
                "--window-ms" => {
                    parsed.absorption.window_ns =
                        parse_next::<u64>(&mut args, "--window-ms")?.saturating_mul(1_000_000)
                }
                "--min-executed-qty" => {
                    parsed.absorption.min_executed_qty =
                        parse_next(&mut args, "--min-executed-qty")?
                }
                "--min-execute-events" => {
                    parsed.absorption.min_execute_events =
                        parse_next(&mut args, "--min-execute-events")?
                }
                "--min-replenished-qty" => {
                    parsed.absorption.min_replenished_qty =
                        parse_next(&mut args, "--min-replenished-qty")?
                }
                "--min-replenishment-ratio-bps" => {
                    parsed.absorption.min_replenishment_ratio_bps =
                        parse_next(&mut args, "--min-replenishment-ratio-bps")?
                }
                "--min-visible-qty-after" => {
                    parsed.absorption.min_visible_qty_after =
                        parse_next(&mut args, "--min-visible-qty-after")?
                }
                "--max-pull-ratio-bps" => {
                    parsed.absorption.max_pull_ratio_bps =
                        parse_next(&mut args, "--max-pull-ratio-bps")?
                }
                "--cooldown-ms" => {
                    parsed.absorption.cooldown_ns =
                        parse_next::<u64>(&mut args, "--cooldown-ms")?.saturating_mul(1_000_000)
                }
                other => anyhow::bail!("unknown argument {other:?}"),
            }
        }

        if parsed.urls.is_empty() {
            usage();
            anyhow::bail!("at least one --url is required");
        }
        if parsed.retain_signals == 0 {
            anyhow::bail!("--retain-signals must be > 0");
        }
        Ok(parsed)
    }
}

fn usage() {
    eprintln!(
        "usage: absorption_sidecar --url ws://host/ws?channel=obo&codec=raw-v1&snapshot=1 [--url ws://host-b/ws?channel=obo&codec=raw-v1&snapshot=1] [options]\n\
options:\n\
  --listen ADDR                         HTTP API bind address (default {DEFAULT_LISTEN})\n\
  --auth-token TOKEN                    bearer token for upstream WebSocket feeds\n\
  --retain-signals N                    recent signals retained by /signals (default {DEFAULT_RETAIN_SIGNALS})\n\
  --record-frames PATH                  write length-prefixed raw-v1 frames for absorption_replay\n\
  --window-ms N                         rolling observation window\n\
  --min-executed-qty N                  minimum executed quantity\n\
  --min-execute-events N                minimum execution events\n\
  --min-replenished-qty N               minimum replenished quantity\n\
  --min-replenishment-ratio-bps N       minimum replenish/executed ratio\n\
  --min-visible-qty-after N             minimum visible quantity after pressure\n\
  --max-pull-ratio-bps N                maximum pull/executed ratio\n\
  --cooldown-ms N                       per-level signal cooldown"
    );
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

fn parse_next<T>(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    next_value(args, flag)?
        .parse::<T>()
        .with_context(|| format!("parse {flag} value"))
}

fn connect_loop(
    url: String,
    auth: Option<String>,
    tx: Sender<Vec<u8>>,
    state: Arc<Mutex<SidecarState>>,
) {
    loop {
        with_state(state.as_ref(), |s| {
            s.connection_attempts = s.connection_attempts.saturating_add(1);
        });
        match connect_once(&url, auth.as_deref(), &tx) {
            Ok(()) => {
                set_last_error(state.as_ref(), format!("websocket {url} closed"));
            }
            Err(err) => {
                with_state(state.as_ref(), |s| {
                    s.websocket_errors = s.websocket_errors.saturating_add(1);
                    s.last_error = Some(format!("websocket {url}: {err:?}"));
                });
            }
        }
        thread::sleep(Duration::from_millis(1_000));
    }
}

fn connect_once(url: &str, auth: Option<&str>, tx: &Sender<Vec<u8>>) -> anyhow::Result<()> {
    let mut req = tungstenite::http::Request::builder().uri(url);
    if let Some(token) = auth {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let req = req.body(())?;
    let (mut ws, _) = tungstenite::connect(req).with_context(|| format!("connect {url}"))?;
    loop {
        match ws.read() {
            Ok(Message::Binary(frame)) => {
                if tx.send(frame).is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(err).context("read websocket message"),
        }
    }
}

fn record_frame_if_enabled(
    recorder: &mut Option<BufWriter<File>>,
    recorded_since_flush: &mut u64,
    frame: &[u8],
    state: &Mutex<SidecarState>,
) {
    let Some(writer) = recorder.as_mut() else {
        return;
    };
    let result = write_recorded_frame(writer, frame).and_then(|()| {
        *recorded_since_flush = recorded_since_flush.saturating_add(1);
        if *recorded_since_flush >= RECORD_FLUSH_INTERVAL {
            *recorded_since_flush = 0;
            writer.flush()?;
        }
        Ok(())
    });
    if let Err(err) = result {
        with_state(state, |s| {
            s.record_write_errors = s.record_write_errors.saturating_add(1);
            s.last_error = Some(format!("record raw frame: {err:?}"));
        });
    }
}

fn write_recorded_frame(writer: &mut BufWriter<File>, frame: &[u8]) -> std::io::Result<()> {
    if frame.len() > MAX_RECORDED_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "raw frame too large to record",
        ));
    }
    let len = frame.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(frame)
}

fn process_frame(
    frame: &[u8],
    detector: &mut AbsorptionDetector,
    dedupe: &mut OboLiveDedupe,
    state: &Mutex<SidecarState>,
) {
    with_state(state, |s| {
        s.frames_received = s.frames_received.saturating_add(1);
        s.last_frame_ns = Some(now_nanos());
    });

    match parse_obo_frame(frame) {
        Ok(Some(parsed)) => {
            if !dedupe.accept(&parsed) {
                with_state(state, |s| {
                    s.duplicate_events = s.duplicate_events.saturating_add(1);
                });
                return;
            }
            with_state(state, |s| {
                s.parsed_events = s.parsed_events.saturating_add(1);
            });
            if let Some(signal) =
                detector.observe_obo(parsed.send_time_ns, parsed.instrument_id, parsed.event)
            {
                if let Ok(line) = serde_json::to_string(&signal) {
                    println!("{line}");
                }
                with_state(state, |s| s.push_signal(signal));
            }
        }
        Ok(None) => {
            with_state(state, |s| {
                s.control_frames = s.control_frames.saturating_add(1);
            });
        }
        Err(err) => {
            with_state(state, |s| {
                s.parse_errors = s.parse_errors.saturating_add(1);
                s.last_error = Some(format!("parse raw-v1 frame: {err}"));
            });
        }
    }
}

fn spawn_http(
    addr: String,
    state: Arc<Mutex<SidecarState>>,
) -> anyhow::Result<thread::JoinHandle<()>> {
    let server = tiny_http::Server::http(&addr)
        .map_err(|err| anyhow::anyhow!("bind absorption API {addr}: {err}"))?;
    let join = thread::Builder::new()
        .name("absorption-api".into())
        .spawn(move || {
            for req in server.incoming_requests() {
                respond(req, state.as_ref());
            }
        })?;
    Ok(join)
}

fn respond(req: tiny_http::Request, state: &Mutex<SidecarState>) {
    let path = req.url().split('?').next().unwrap_or(req.url());
    match path {
        "/" => {
            let body = "endpoints: /healthz /ready /stats /signals\n";
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(200));
        }
        "/healthz" => {
            let _ = req.respond(tiny_http::Response::from_string("OK").with_status_code(200));
        }
        "/ready" => {
            let ready = with_state_result(state, |s| s.frames_received > 0);
            let status = if ready { 200 } else { 503 };
            let body = if ready { "READY" } else { "NOT_READY" };
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(status));
        }
        "/stats" => {
            let stats = with_state_result(state, SidecarState::stats);
            let _ = req.respond(json_response(&stats));
        }
        "/signals" => {
            let signals = with_state_result(state, SidecarState::signals);
            let _ = req.respond(json_response(&SignalsResponse { signals }));
        }
        _ => {
            let _ = req.respond(tiny_http::Response::empty(404));
        }
    }
}

fn json_response<T: Serialize>(value: &T) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec());
    tiny_http::Response::from_data(body)
        .with_status_code(200)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
        )
}

impl SidecarState {
    fn new(retain_signals: usize) -> Self {
        Self {
            retain_signals,
            started_at_ns: now_nanos(),
            frames_received: 0,
            control_frames: 0,
            parsed_events: 0,
            duplicate_events: 0,
            parse_errors: 0,
            signals_emitted: 0,
            connection_attempts: 0,
            websocket_errors: 0,
            record_write_errors: 0,
            last_frame_ns: None,
            last_signal_ns: None,
            last_error: None,
            recent_signals: VecDeque::with_capacity(retain_signals),
        }
    }

    fn push_signal(&mut self, signal: AbsorptionSignal) {
        self.signals_emitted = self.signals_emitted.saturating_add(1);
        self.last_signal_ns = Some(signal.window_end_ns);
        if self.recent_signals.len() == self.retain_signals {
            self.recent_signals.pop_front();
        }
        self.recent_signals.push_back(signal);
    }

    fn stats(&self) -> StatsResponse {
        StatsResponse {
            started_at_ns: self.started_at_ns,
            frames_received: self.frames_received,
            control_frames: self.control_frames,
            parsed_events: self.parsed_events,
            duplicate_events: self.duplicate_events,
            parse_errors: self.parse_errors,
            signals_emitted: self.signals_emitted,
            connection_attempts: self.connection_attempts,
            websocket_errors: self.websocket_errors,
            record_write_errors: self.record_write_errors,
            last_frame_ns: self.last_frame_ns,
            last_signal_ns: self.last_signal_ns,
            recent_signal_count: self.recent_signals.len(),
            last_error: self.last_error.clone(),
        }
    }

    fn signals(&self) -> Vec<AbsorptionSignal> {
        self.recent_signals.iter().cloned().collect()
    }
}

fn with_state(state: &Mutex<SidecarState>, f: impl FnOnce(&mut SidecarState)) {
    if let Ok(mut guard) = state.lock() {
        f(&mut guard);
    }
}

fn with_state_result<T>(state: &Mutex<SidecarState>, f: impl FnOnce(&SidecarState) -> T) -> T {
    let guard = state.lock().expect("sidecar state mutex poisoned");
    f(&guard)
}

fn set_last_error(state: &Mutex<SidecarState>, err: String) {
    with_state(state, |s| {
        s.last_error = Some(err);
    });
}
