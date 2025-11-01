// Binary wire format (raw v1) for OBO and control frames
// - Little-endian
// - #[repr(C)] with zerocopy for safe cast to/from bytes

use zerocopy::{AsBytes, FromBytes, Unaligned};

pub const MAGIC: [u8; 4] = *b"OBv1";
pub const VERSION_V1: u8 = 1;

// Codec identifiers
pub mod codec {
    pub const RAW_V1: u8 = 0; // 0 = raw structs (v1)
    // Future: 1 = SBE
}

// Channel identifiers
pub mod channel_id {
    pub const OBO_L3: u32 = 0;
}

// Message type identifiers (u16)
pub mod msg_type {
    // Control
    pub const HEARTBEAT: u16 = 1;
    pub const GAP: u16 = 2;
    pub const SNAPSHOT_START: u16 = 3;
    pub const SNAPSHOT_END: u16 = 4;
    pub const SEQ_RESET: u16 = 5;

    // OBO events
    pub const OBO_ADD: u16 = 100;
    pub const OBO_MODIFY: u16 = 101;
    pub const OBO_CANCEL: u16 = 102;
    pub const OBO_EXECUTE: u16 = 103;
    pub const SNAPSHOT_HDR: u16 = 104; // FullBookSnapshotHdrV1
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct FrameHeaderV1 {
    pub magic: [u8; 4],
    pub version: u8,
    pub codec: u8,
    pub message_type: u16,
    pub channel_id: u32,
    pub instrument_id: u64,
    pub sequence: u64,
    pub send_time_ns: u64,
    pub payload_len: u32,
}

// --------------------------- Control Payloads ----------------------------

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct HeartbeatV1 {
    pub reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct GapV1 {
    pub from_inclusive: u64,
    pub to_inclusive: u64,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct SnapshotStartV1 {
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct SnapshotEndV1 {
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct SeqResetV1 {
    pub new_start_seq: u64,
}

// --------------------------- OBO Payloads -------------------------------

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct OboAddV1 {
    pub order_id: u64,
    pub price_e8: i64,
    pub qty: u64,
    pub side: u8,  // 0 = Bid, 1 = Ask
    pub flags: u8, // reserved for future
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct OboModifyV1 {
    pub order_id: u64,
    pub new_price_e8: i64,
    pub new_qty: u64,
    pub flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct OboCancelV1 {
    pub order_id: u64,
    pub qty_cxl: u64,
    pub reason: u8, // 0=Unknown/Other
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct OboExecuteV1 {
    pub maker_order_id: u64,
    pub trade_qty: u64,
    pub trade_price_e8: i64,
    pub aggressor_side: u8, // 0 = Bid, 1 = Ask (aggressor)
    pub match_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, AsBytes, Unaligned)]
pub struct FullBookSnapshotHdrV1 {
    pub level_count: u32,
    pub total_orders: u32,
}


