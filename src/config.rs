// src/config.rs 
use serde::Deserialize;
use std::{fs, net::Ipv4Addr, path::Path};

 
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub general: General,
    pub sequence: Sequence,
    pub parser: Parser,
    pub channels: Channels,
    pub merge: Merge,
    pub book: Book,
    pub cpu: Cpu,
    pub metrics: Option<Metrics>,
    pub snapshot: Option<SnapshotCfg>,
    pub recovery: Option<RecoveryCfg>,
    pub afxdp: Option<AfxdpCfg>,
    #[serde(default)]
    pub feeds: Option<Feeds>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct General {
    pub max_packet_size: u32,       // e.g., 2048
    pub pool_size: usize,           // e.g., 65536
    pub rx_queue_capacity: usize,   // e.g., 65536
    pub merge_queue_capacity: usize,// e.g., 65536
    pub spin_loops_per_yield: u32,  // e.g., 64
    #[serde(default)]
    pub rx_recvmmsg_batch: Option<usize>, // if Some(N>1), enable batched recvmmsg
    #[serde(default)]
    pub mlock_all: bool,            // mlockall current+future (Linux; best-effort)
    #[serde(default)]
    pub json_logs: bool,            // structured JSON logs to stdout
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sequence {
    pub offset: u16,                // bytes into packet payload
    pub length: u8,                 // 4 or 8 for u32/u64
    pub endian: Endian,             // "be" or "le"
}

#[derive(Debug, Clone, Deserialize)]
pub struct Parser {
    pub kind: ParserKind,
    pub max_messages_per_packet: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserKind {
    #[serde(rename = "fixed_binary")]
    FixedBinary,
    #[serde(rename = "fast_like")]
    FastLike,
    #[serde(rename = "itch50")]
    Itch50,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian { Be, Le }

#[derive(Debug, Clone, Deserialize)]
pub struct Channels {
    pub a: ChannelCfg,
    pub b: ChannelCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelCfg {
    pub group: Ipv4Addr,            // e.g., 239.10.10.1
    pub port: u16,                  // e.g., 5001
    pub iface_addr: Ipv4Addr,       // local interface IPv4 of the NIC to join on
    pub reuse_port: bool,
    pub recv_buffer_bytes: u32,     // e.g., 64<<20
    #[allow(dead_code)]
    pub busy_poll_us: Option<u32>,  // Linux SO_BUSY_POLL (optional)
    pub nonblocking: bool,          // true for busy-spin recv path
    #[serde(default)]
    pub timestamping: Option<TimestampingMode>, // default Off
    #[serde(default)]
    pub workers: Option<usize>,     // per-channel UDP RX sockets/threads (requires reuse_port)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Merge {
    pub initial_expected_seq: u64,
    pub reorder_window: u64,        // window for out-of-order buffering
    pub max_pending_packets: usize, // hard cap for pending map
    #[serde(default)]
    pub dwell_ns: Option<u64>,      // preferred minimum dwell between A/B switches
    #[serde(default)]
    pub adaptive: bool,             // enable adaptive reorder window tuning
    #[serde(default)]
    pub reorder_window_max: Option<u64>, // cap for adaptive window
}

#[derive(Debug, Clone, Deserialize)]
pub struct Book {
    pub max_depth: usize,           // reporting depth (snapshots/logs)
    pub snapshot_interval_ms: u64,  // periodic snapshot/logging cadence
    #[serde(default)]
    pub consume_trades: bool,       // whether to reduce book on trades when feed omits mods/dels
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecoveryCfg {
    /// Enable TCP replay injector; else logger-only
    pub enable_injector: bool,
    /// TCP endpoint of replay service (e.g. "10.0.0.1:9000")
    pub endpoint: String,
    #[serde(default)]
    /// Optional path to append-only backlog of gap requests
    pub backlog_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cpu {
    pub a_rx_core: Option<usize>,
    pub b_rx_core: Option<usize>,
    pub merge_core: Option<usize>,
    pub decode_core: Option<usize>,
    #[serde(default)]
    pub rt_priority: Option<i32>,   // SCHED_FIFO priority if set (Linux)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metrics {
    /// Bind address for Prometheus exporter (e.g. "0.0.0.0:9100")
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotCfg {
    /// Snapshot file path (e.g. "/var/lib/t7_like/book.snap")
    pub path: String,
    /// Attempt to load snapshot at startup (if present)
    pub load_on_start: bool,
    /// Enable periodic snapshot writing
    pub enable_writer: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AfxdpCfg {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_ifname")]
    pub ifname: String,
    #[serde(default)]
    pub queue_id: u32,
    #[serde(default)]
    /// Number of RX queues (RSS) to spawn when using AF_XDP/AF_PACKET ring
    pub queues: Option<usize>,
    #[serde(default)]
    /// UMEM frame size (bytes) for AF_XDP (hint; fallback path ignores)
    pub umem_frame_size: Option<usize>,
    #[serde(default)]
    /// UMEM total frames for AF_XDP (hint; fallback path ignores)
    pub umem_frame_count: Option<usize>,
}

fn default_ifname() -> String { "eth0".to_string() }

impl AppConfig {
    pub fn from_file(p: &Path) -> anyhow::Result<Self> {
        let s = fs::read_to_string(p)?;
        let cfg: AppConfig = toml::from_str(&s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.channels.a.group.is_multicast() || !self.channels.b.group.is_multicast() {
            anyhow::bail!("channels.a.group and channels.b.group must be multicast IPv4 addresses");
        }
        if self.sequence.length != 4 && self.sequence.length != 8 {
            anyhow::bail!("sequence.length must be 4 or 8");
        }
        if self.general.max_packet_size < 512 || self.general.max_packet_size > 65535 {
            anyhow::bail!("general.max_packet_size must be in [512, 65535]");
        }
        if self.merge.reorder_window == 0 {
            anyhow::bail!("merge.reorder_window must be > 0");
        }
        if self.channels.a.workers.unwrap_or(1) > 1 && !self.channels.a.reuse_port {
            anyhow::bail!("channels.a.workers > 1 requires reuse_port = true");
        }
        if self.channels.b.workers.unwrap_or(1) > 1 && !self.channels.b.reuse_port {
            anyhow::bail!("channels.b.workers > 1 requires reuse_port = true");
        }
        if let Some(ref feeds) = self.feeds {
            for p in &feeds.pops {
                if p.ws_endpoints.len() != 2 { anyhow::bail!("each pop.ws_endpoints must have 2 entries"); }
                if p.h3_endpoints.len() != 2 { anyhow::bail!("each pop.h3_endpoints must have 2 entries"); }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampingMode {
    Off,
    Software,
    Hardware,
    HardwareRaw,
}

// ---------- Feeds / Publishers ----------

#[derive(Debug, Clone, Deserialize)]
pub struct Feeds {
    #[serde(default)]
    pub enabled: bool,
    pub pops: Vec<Pop>,
    #[serde(default)]
    pub tls: Option<TlsCfg>,
    #[serde(default)]
    pub obo: Option<OboFeedCfg>,
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pop {
    pub name: String,
    pub ws_endpoints: Vec<String>, // two endpoints per POP
    pub h3_endpoints: Vec<String>, // two endpoints per POP
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsCfg {
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OboFeedCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub symbols: Option<Vec<String>>, // optional filter
    #[serde(default)]
    pub buffers: Option<BuffersCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuffersCfg {
    #[serde(default = "default_pub_queue")] pub pub_queue: usize,
    #[serde(default = "default_client_max_lag_frames")] pub client_max_lag_frames: usize,
}

fn default_pub_queue() -> usize { 65536 }
fn default_client_max_lag_frames() -> usize { 16384 }