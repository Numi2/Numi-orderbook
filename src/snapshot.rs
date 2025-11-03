// src/snapshot.rs
use crate::orderbook::{BookExport, OrderBook};
use anyhow::Context;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

const MAGIC: &[u8; 8] = b"OBSNAP\0\0";
const VERSION: u32 = 1;

pub fn write_atomic(path: &Path, export: &BookExport) -> anyhow::Result<()> {
    let mut payload = Vec::with_capacity(1024 * 1024);
    // header
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&VERSION.to_be_bytes());
    let ts_ns = current_time_ns();
    payload.extend_from_slice(&ts_ns.to_be_bytes());
    // body
    let body = bincode::serialize(export)?;
    payload.extend_from_slice(&body);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp).with_context(|| format!("create tmp snapshot {:?}", tmp))?;
        f.write_all(&payload)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

pub fn load(path: &Path) -> anyhow::Result<OrderBook> {
    let mut f = File::open(path).with_context(|| format!("open snapshot {:?}", path))?;
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    if v.len() < 8 + 4 + 8 {
        anyhow::bail!("snapshot too small");
    }
    if &v[0..8] != MAGIC {
        anyhow::bail!("bad snapshot magic");
    }
    let ver = u32::from_be_bytes([v[8], v[9], v[10], v[11]]);
    if ver != VERSION {
        anyhow::bail!("unsupported snapshot version: {}", ver);
    }
    // let ts_ns = u64::from_be_bytes(v[12..20].try_into().unwrap()); // available if needed
    let body = &v[20..];
    let export: BookExport = bincode::deserialize(body)?;
    Ok(OrderBook::from_export(export))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
    p.set_extension(format!("{ext}.partial"));
    p
}

fn current_time_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    d.as_secs() * 1_000_000_000 + u64::from(d.subsec_nanos())
}

use crossbeam_channel::{Receiver, Sender};

pub struct SnapshotWriter {
    _tx: Sender<BookExport>,
    join: thread::JoinHandle<()>,
}

impl SnapshotWriter {
    pub fn spawn(path: PathBuf) -> (Sender<BookExport>, SnapshotWriter) {
        let (tx, rx) = crossbeam_channel::bounded::<BookExport>(2);
        let join = thread::Builder::new()
            .name("snapshot-writer".into())
            .spawn(move || run_writer(path, rx))
            .expect("spawn snapshot writer");
        (tx.clone(), SnapshotWriter { _tx: tx, join })
    }

    pub fn join(self) {
        let _ = self.join.join();
    }
}

fn run_writer(path: PathBuf, rx: Receiver<BookExport>) {
    log::info!("snapshot writer started -> {:?}", path);
    while let Ok(export) = rx.recv() {
        if let Err(e) = write_atomic(&path, &export) {
            log::error!("snapshot write failed: {e:?}");
        } else {
            log::debug!("snapshot written to {:?}", path);
        }
    }
}
