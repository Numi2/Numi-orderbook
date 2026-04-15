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
    #[serde(default)]
    pub journal: Option<JournalCfg>,
    pub recovery: Option<RecoveryCfg>,
    pub afxdp: Option<AfxdpCfg>,
    #[serde(default)]
    pub packet_mmap: Option<PacketMmapCfg>,
    #[serde(default)]
    pub feeds: Option<Feeds>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct General {
    pub max_packet_size: u32,        // e.g., 2048
    pub pool_size: usize,            // e.g., 65536
    pub rx_queue_capacity: usize,    // e.g., 65536
    pub merge_queue_capacity: usize, // e.g., 65536
    pub spin_loops_per_yield: u32,   // e.g., 64
    #[serde(default)]
    pub rx_recvmmsg_batch: Option<usize>, // if Some(N>1), enable batched recvmmsg
    #[serde(default)]
    pub mlock_all: bool, // fail-fast mlockall current+future on Linux
    #[serde(default)]
    pub json_logs: bool, // structured JSON logs to stdout
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sequence {
    pub offset: u16,    // bytes into packet payload
    pub length: u8,     // 4 or 8 for u32/u64
    pub endian: Endian, // "be" or "le"
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
pub enum Endian {
    Be,
    Le,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channels {
    pub a: ChannelCfg,
    pub b: ChannelCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelCfg {
    pub group: Ipv4Addr,      // e.g., 239.10.10.1
    pub port: u16,            // e.g., 5001
    pub iface_addr: Ipv4Addr, // local interface IPv4 of the NIC to join on
    pub reuse_port: bool,
    pub recv_buffer_bytes: u32,    // e.g., 64<<20
    pub busy_poll_us: Option<u32>, // Linux SO_BUSY_POLL (optional)
    pub nonblocking: bool,         // true for busy-spin recv path
    #[serde(default)]
    pub timestamping: Option<TimestampingMode>, // default Off
    #[serde(default)]
    pub workers: Option<usize>, // per-channel UDP RX sockets/threads (requires reuse_port)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Merge {
    pub initial_expected_seq: u64,
    pub reorder_window: u64,        // window for out-of-order buffering
    pub max_pending_packets: usize, // hard cap for pending map
    #[serde(default)]
    pub dwell_ns: Option<u64>, // preferred minimum dwell between A/B switches
    #[serde(default)]
    pub adaptive: bool, // enable adaptive reorder window tuning
    #[serde(default)]
    pub reorder_window_max: Option<u64>, // cap for adaptive window
}

#[derive(Debug, Clone, Deserialize)]
pub struct Book {
    pub max_depth: usize,          // reporting depth (snapshots/logs)
    pub snapshot_interval_ms: u64, // periodic snapshot/logging cadence
    #[serde(default)]
    pub consume_trades: bool, // whether to reduce book on trades when feed omits mods/dels
    #[serde(default = "default_grid_tick")]
    pub default_tick: i64,
    #[serde(default = "default_grid_span")]
    pub grid_span: usize,
    #[serde(default = "default_order_slab_capacity")]
    pub order_slab_capacity: usize,
    #[serde(default)]
    pub instrument_ticks: Vec<InstrumentTickCfg>,
    #[serde(default)]
    pub instrument_ticks_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentTickCfg {
    pub instr: u32,
    pub tick: i64,
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
    #[serde(default)]
    /// Number of replay fetch attempts per coalesced gap range.
    pub retry_attempts: Option<u32>,
    #[serde(default)]
    /// Linear retry backoff in milliseconds; attempt N waits N * retry_backoff_ms.
    pub retry_backoff_ms: Option<u64>,
    #[serde(default)]
    /// Minimum delay between replay fetch attempts, for venue request-rate limits.
    pub min_request_interval_ms: Option<u64>,
    #[serde(default)]
    /// Recovery range SLO in milliseconds; 0 disables SLO violation reporting.
    pub slo_ms: Option<u64>,
    #[serde(default)]
    /// Escalation behavior when a range exhausts configured replay attempts.
    pub unrecoverable_policy: Option<crate::recovery::UnrecoverablePolicy>,
    #[serde(default)]
    /// TCP read/write timeout for each replay request; 0 disables explicit timeout.
    pub request_timeout_ms: Option<u64>,
    #[serde(default)]
    /// Replay protocol adapter used by the TCP injector.
    pub replay_protocol: Option<crate::recovery::ReplayProtocol>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cpu {
    pub a_rx_core: Option<usize>,
    pub b_rx_core: Option<usize>,
    pub merge_core: Option<usize>,
    pub decode_core: Option<usize>,
    #[serde(default)]
    pub rt_priority: Option<i32>, // SCHED_FIFO priority if set (Linux)
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
pub struct JournalCfg {
    /// Append-only event journal path.
    pub path: String,
    /// Enable live journal writing from the decode thread.
    pub enable_writer: bool,
    /// Record post-event state hashes for deterministic replay checks.
    #[serde(default = "default_journal_record_state_hash")]
    pub record_state_hash: bool,
}

fn default_journal_record_state_hash() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct AfxdpCfg {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_ifname")]
    pub ifname: String,
    #[serde(default)]
    pub queues: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PacketMmapCfg {
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_ifname")]
    pub ifname: String,
    #[serde(default)]
    /// Number of RX queues to spawn when using PACKET_RX_RING.
    pub queues: Option<usize>,
    #[serde(default = "default_packet_mmap_frame_size")]
    pub frame_size: u32,
    #[serde(default = "default_packet_mmap_frames_per_block")]
    pub frames_per_block: u32,
    #[serde(default = "default_packet_mmap_block_count")]
    pub block_count: u32,
}

fn default_ifname() -> String {
    "eth0".to_string()
}

fn default_grid_tick() -> i64 {
    1
}

fn default_grid_span() -> usize {
    16384
}

fn default_order_slab_capacity() -> usize {
    1 << 20
}

fn default_packet_mmap_frame_size() -> u32 {
    2048
}

fn default_packet_mmap_frames_per_block() -> u32 {
    1024
}

fn default_packet_mmap_block_count() -> u32 {
    4
}

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
        // Book constraints
        if self.book.max_depth == 0 {
            anyhow::bail!("book.max_depth must be > 0");
        }
        if self.book.snapshot_interval_ms == 0 {
            anyhow::bail!("book.snapshot_interval_ms must be > 0");
        }
        if self.book.default_tick <= 0 {
            anyhow::bail!("book.default_tick must be > 0");
        }
        if self.book.grid_span == 0 {
            anyhow::bail!("book.grid_span must be > 0");
        }
        if self.book.order_slab_capacity == 0 {
            anyhow::bail!("book.order_slab_capacity must be > 0");
        }
        for tick in &self.book.instrument_ticks {
            if tick.tick <= 0 {
                anyhow::bail!("book.instrument_ticks tick must be > 0");
            }
        }
        if let Some(ref path) = self.book.instrument_ticks_path {
            if path.trim().is_empty() {
                anyhow::bail!("book.instrument_ticks_path must be non-empty if set");
            }
        }
        if let Some(ref feeds) = self.feeds {
            for p in &feeds.pops {
                if p.ws_endpoints.len() != 2 {
                    anyhow::bail!("each pop.ws_endpoints must have 2 entries");
                }
            }
            // Basic feeds validation and field reads
            if feeds.enabled && feeds.pops.is_empty() {
                anyhow::bail!("feeds.enabled = true requires at least one POP");
            }
            if let Some(ref tok) = feeds.auth_token {
                if tok.trim().is_empty() {
                    anyhow::bail!("feeds.auth_token, if set, must be non-empty");
                }
            }
            if let Some(ref obo) = feeds.obo {
                if let Some(ref bufs) = obo.buffers {
                    if bufs.pub_queue == 0 {
                        anyhow::bail!("feeds.obo.buffers.pub_queue must be > 0");
                    }
                }
                if obo.client_write_timeout_ms == 0 {
                    anyhow::bail!("feeds.obo.client_write_timeout_ms must be > 0");
                }
                if obo.client_handshake_timeout_ms == 0 {
                    anyhow::bail!("feeds.obo.client_handshake_timeout_ms must be > 0");
                }
                if obo.client_heartbeat_interval_ms == 0 {
                    anyhow::bail!("feeds.obo.client_heartbeat_interval_ms must be > 0");
                }
                if obo.client_max_connections == 0 {
                    anyhow::bail!("feeds.obo.client_max_connections must be > 0");
                }
            }
        }
        // Snapshot cfg
        if let Some(ref s) = self.snapshot {
            if s.path.trim().is_empty() {
                anyhow::bail!("snapshot.path must be non-empty when snapshot is configured");
            }
        }
        if let Some(ref j) = self.journal {
            if j.enable_writer && j.path.trim().is_empty() {
                anyhow::bail!("journal.path must be non-empty when journal writer is enabled");
            }
        }
        // Recovery cfg
        if let Some(ref r) = self.recovery {
            if r.enable_injector && (r.endpoint.trim().is_empty() || !r.endpoint.contains(':')) {
                anyhow::bail!("recovery.endpoint must be host:port when enable_injector = true");
            }
            if let Some(ref path) = r.backlog_path {
                if path.trim().is_empty() {
                    anyhow::bail!("recovery.backlog_path must be non-empty if set");
                }
            }
            if r.retry_attempts == Some(0) {
                anyhow::bail!("recovery.retry_attempts must be > 0 if set");
            }
        }
        // AF_XDP cfg (if present)
        if let Some(ref a) = self.afxdp {
            if a.enable {
                anyhow::bail!(
                    "afxdp.enable requires a real AF_XDP/XSK backend; no incomplete AF_XDP receive path is available"
                );
            }
            if a.ifname.trim().is_empty() {
                anyhow::bail!("afxdp.ifname must be non-empty if afxdp is configured");
            }
            if a.queues == Some(0) {
                anyhow::bail!("afxdp.queues must be > 0 if set");
            }
        }
        if let Some(ref p) = self.packet_mmap {
            if p.ifname.trim().is_empty() {
                anyhow::bail!("packet_mmap.ifname must be non-empty if packet_mmap is configured");
            }
            if p.queues == Some(0) {
                anyhow::bail!("packet_mmap.queues must be > 0 if set");
            }
            if p.frame_size < 2048 || !p.frame_size.is_power_of_two() {
                anyhow::bail!("packet_mmap.frame_size must be a power of two and at least 2048");
            }
            if p.frames_per_block == 0 {
                anyhow::bail!("packet_mmap.frames_per_block must be > 0");
            }
            if p.block_count == 0 {
                anyhow::bail!("packet_mmap.block_count must be > 0");
            }
            p.frame_size
                .checked_mul(p.frames_per_block)
                .ok_or_else(|| anyhow::anyhow!("packet_mmap block size overflow"))?;
            p.frames_per_block
                .checked_mul(p.block_count)
                .ok_or_else(|| anyhow::anyhow!("packet_mmap frame count overflow"))?;
        }
        if self.afxdp.as_ref().map(|c| c.enable).unwrap_or(false)
            && self.packet_mmap.as_ref().map(|c| c.enable).unwrap_or(false)
        {
            anyhow::bail!("afxdp.enable and packet_mmap.enable cannot both be true");
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
#[serde(deny_unknown_fields)]
pub struct Feeds {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub pops: Vec<Pop>,
    #[serde(default)]
    pub obo: Option<OboFeedCfg>,
    #[serde(default)]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pop {
    pub ws_endpoints: Vec<String>, // two endpoints per POP
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OboFeedCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub buffers: Option<BuffersCfg>,
    #[serde(default = "default_client_write_timeout_ms")]
    pub client_write_timeout_ms: u64,
    #[serde(default = "default_client_handshake_timeout_ms")]
    pub client_handshake_timeout_ms: u64,
    #[serde(default = "default_client_heartbeat_interval_ms")]
    pub client_heartbeat_interval_ms: u64,
    #[serde(default = "default_client_max_connections")]
    pub client_max_connections: usize,
    #[serde(default = "default_client_nodelay")]
    pub client_nodelay: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuffersCfg {
    #[serde(default = "default_pub_queue")]
    pub pub_queue: usize,
}

fn default_pub_queue() -> usize {
    65536
}

fn default_client_write_timeout_ms() -> u64 {
    250
}

fn default_client_handshake_timeout_ms() -> u64 {
    1_000
}

fn default_client_heartbeat_interval_ms() -> u64 {
    1_000
}

fn default_client_max_connections() -> usize {
    1024
}

fn default_client_nodelay() -> bool {
    true
}
