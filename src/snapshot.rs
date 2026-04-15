// src/snapshot.rs
use crate::orderbook::{BookExport, OrderBook};
use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

const MAGIC: &[u8; 8] = b"OBSNAP\0\0";
const VERSION_V1: u32 = 1;
const VERSION_V2: u32 = 2;

pub fn write_atomic(path: &Path, image: &SnapshotImage) -> anyhow::Result<()> {
    let mut payload = Vec::with_capacity(1024 * 1024);
    // header
    payload.extend_from_slice(MAGIC);
    payload.extend_from_slice(&VERSION_V2.to_be_bytes());
    let ts_ns = current_time_ns();
    payload.extend_from_slice(&ts_ns.to_be_bytes());
    payload.extend_from_slice(&image.replay_from.unwrap_or(u64::MAX).to_be_bytes());
    // body
    let body = bincode::serialize(&image.export)?;
    let cap_before_body = payload.capacity();
    payload.extend_from_slice(&body);
    if payload.capacity() > cap_before_body {
        crate::metrics::inc_snapshot_payload_vec_grow();
    }
    crate::metrics::set_snapshot_payload_bytes(payload.len());

    if let Some(dir) = parent_dir(path) {
        fs::create_dir_all(dir).with_context(|| format!("create snapshot dir {:?}", dir))?;
    }
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp).with_context(|| format!("create tmp snapshot {:?}", tmp))?;
        f.write_all(&payload)?;
        f.sync_all()
            .with_context(|| format!("sync tmp snapshot {:?}", tmp))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    sync_parent_dir(path)?;
    Ok(())
}

pub fn load(path: &Path) -> anyhow::Result<OrderBook> {
    Ok(load_image(path)?.book)
}

pub fn load_image(path: &Path) -> anyhow::Result<LoadedSnapshot> {
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
    let body_start = match ver {
        VERSION_V1 => 20,
        VERSION_V2 => {
            if v.len() < 28 {
                anyhow::bail!("snapshot v2 too small");
            }
            28
        }
        _ => anyhow::bail!("unsupported snapshot version: {}", ver),
    };
    let replay_from = if ver == VERSION_V2 {
        let raw = u64::from_be_bytes(v[20..28].try_into().unwrap());
        (raw != u64::MAX).then_some(raw)
    } else {
        None
    };
    let body = &v[body_start..];
    let export: BookExport = bincode::deserialize(body)?;
    Ok(LoadedSnapshot {
        book: OrderBook::from_export(export),
        replay_from,
    })
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
    p.set_extension(format!("{ext}.partial"));
    p
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|dir| !dir.as_os_str().is_empty())
}

#[cfg(target_os = "linux")]
fn sync_parent_dir(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = parent_dir(path) {
        let f = File::open(dir).with_context(|| format!("open snapshot dir {:?}", dir))?;
        f.sync_all()
            .with_context(|| format!("sync snapshot dir {:?}", dir))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn sync_parent_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn current_time_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    d.as_secs() * 1_000_000_000 + u64::from(d.subsec_nanos())
}

#[derive(Debug, Clone)]
pub struct SnapshotImage {
    pub export: BookExport,
    pub replay_from: Option<u64>,
}

#[derive(Debug)]
pub struct LoadedSnapshot {
    pub book: OrderBook,
    pub replay_from: Option<u64>,
}

pub struct SnapshotWriter {
    tx: Sender<SnapshotImage>,
    join: thread::JoinHandle<()>,
}

impl SnapshotWriter {
    pub fn spawn(path: PathBuf) -> (Sender<SnapshotImage>, SnapshotWriter) {
        let (tx, rx) = crossbeam_channel::bounded::<SnapshotImage>(2);
        let join = thread::Builder::new()
            .name("snapshot-writer".into())
            .spawn(move || run_writer(path, rx))
            .expect("spawn snapshot writer");
        (tx.clone(), SnapshotWriter { tx, join })
    }

    pub fn join(self) {
        let SnapshotWriter { tx, join } = self;
        drop(tx);
        if join.join().is_err() {
            log::error!("snapshot writer thread panicked");
        }
    }
}

fn run_writer(path: PathBuf, rx: Receiver<SnapshotImage>) {
    log::info!("snapshot writer started -> {:?}", path);
    while let Ok(image) = rx.recv() {
        if let Err(e) = write_atomic(&path, &image) {
            log::error!("snapshot write failed: {e:?}");
        } else {
            log::debug!("snapshot written to {:?}", path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_v2_roundtrips_replay_cursor() {
        let path = std::env::temp_dir().join(format!(
            "numi-orderbook-snapshot-{}-{}.snap",
            std::process::id(),
            current_time_ns()
        ));
        let image = SnapshotImage {
            export: BookExport {
                version: 1,
                instruments: Vec::new(),
            },
            replay_from: Some(42),
        };

        write_atomic(&path, &image).unwrap();
        let loaded = load_image(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.replay_from, Some(42));
        assert_eq!(loaded.book.order_count(), 0);
    }

    #[test]
    fn snapshot_v1_loads_without_replay_cursor() {
        let path = std::env::temp_dir().join(format!(
            "numi-orderbook-snapshot-v1-{}-{}.snap",
            std::process::id(),
            current_time_ns()
        ));
        let export = BookExport {
            version: 1,
            instruments: Vec::new(),
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(MAGIC);
        payload.extend_from_slice(&VERSION_V1.to_be_bytes());
        payload.extend_from_slice(&current_time_ns().to_be_bytes());
        payload.extend_from_slice(&bincode::serialize(&export).unwrap());

        let mut file = File::create(&path).unwrap();
        file.write_all(&payload).unwrap();
        drop(file);

        let loaded = load_image(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.replay_from, None);
        assert_eq!(loaded.book.order_count(), 0);
    }
}
