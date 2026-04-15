use crate::codec_raw::{
    self, channel_id, msg_type, OboAddV1, OboCancelV1, OboExecuteV1, OboModifyV1,
};
use crate::obo::OboEventV1;
use crate::parser::Side;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

const FRAME_HEADER_LEN: usize = 48;
const REPLAY_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const REPLAY_HASH_PRIME: u64 = 0x00000100000001b3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelKey {
    pub instrument_id: u64,
    pub price: i64,
    pub side: Side,
}

impl Hash for LevelKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.instrument_id.hash(state);
        self.price.hash(state);
        side_to_u8(self.side).hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderState {
    instrument_id: u64,
    price: i64,
    qty: u64,
    side: Side,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationKind {
    Execute,
    Replenish,
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LevelObservation {
    ts_ns: u64,
    qty: u64,
    kind: ObservationKind,
}

#[derive(Debug, Default)]
struct LevelState {
    visible_qty: u64,
    observations: VecDeque<LevelObservation>,
    last_signal_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsorptionConfig {
    pub window_ns: u64,
    pub min_executed_qty: u64,
    pub min_execute_events: u32,
    pub min_replenished_qty: u64,
    pub min_replenishment_ratio_bps: u32,
    pub min_visible_qty_after: u64,
    pub max_pull_ratio_bps: u32,
    pub cooldown_ns: u64,
}

impl Default for AbsorptionConfig {
    fn default() -> Self {
        Self {
            window_ns: 2_000_000_000,
            min_executed_qty: 100,
            min_execute_events: 2,
            min_replenished_qty: 25,
            min_replenishment_ratio_bps: 5_000,
            min_visible_qty_after: 50,
            max_pull_ratio_bps: 2_500,
            cooldown_ns: 1_000_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsorptionSignal {
    pub instrument_id: u64,
    pub price: i64,
    pub passive_side: Side,
    pub aggressor_side: Side,
    pub window_start_ns: u64,
    pub window_end_ns: u64,
    pub executed_qty: u64,
    pub replenished_qty: u64,
    pub pulled_qty: u64,
    pub visible_qty_after: u64,
    pub execute_events: u32,
    pub replenish_events: u32,
    pub pull_events: u32,
    pub replenishment_ratio_bps: u32,
    pub pull_ratio_bps: u32,
    pub confidence_bps: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct ParsedOboEvent {
    pub instrument_id: u64,
    pub sequence: u64,
    pub global_sequence: u64,
    pub send_time_ns: u64,
    pub event: OboEventV1,
}

#[derive(Debug, Default)]
pub struct OboLiveDedupe {
    last_seq_by_instr: HashMap<u64, u64>,
}

impl OboLiveDedupe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, event: &ParsedOboEvent) -> bool {
        if event.sequence == 0 {
            return true;
        }
        let last = self
            .last_seq_by_instr
            .entry(event.instrument_id)
            .or_insert(0);
        if event.sequence <= *last {
            return false;
        }
        *last = event.sequence;
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OboFrameError {
    #[error("frame shorter than raw-v1 header")]
    ShortHeader,
    #[error("invalid raw-v1 magic")]
    InvalidMagic,
    #[error("unsupported raw-v1 version: {0}")]
    UnsupportedVersion(u8),
    #[error("unsupported raw-v1 codec: {0}")]
    UnsupportedCodec(u8),
    #[error("unsupported raw-v1 channel: {0}")]
    UnsupportedChannel(u32),
    #[error("payload length mismatch: expected {expected}, actual {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },
    #[error("invalid payload length for message type {message_type}: expected {expected}, actual {actual}")]
    InvalidPayloadLength {
        message_type: u16,
        expected: usize,
        actual: usize,
    },
    #[error("unsupported raw-v1 message type: {0}")]
    UnsupportedMessageType(u16),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsorptionReplayReport {
    pub frames_total: u64,
    pub control_frames: u64,
    pub parsed_events: u64,
    pub duplicate_events: u64,
    pub parse_errors: u64,
    pub signals: u64,
    pub signal_hash: u64,
    pub first_signal_ns: Option<u64>,
    pub last_signal_ns: Option<u64>,
}

impl Default for AbsorptionReplayReport {
    fn default() -> Self {
        Self {
            frames_total: 0,
            control_frames: 0,
            parsed_events: 0,
            duplicate_events: 0,
            parse_errors: 0,
            signals: 0,
            signal_hash: REPLAY_HASH_OFFSET,
            first_signal_ns: None,
            last_signal_ns: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsorptionReplayValidation {
    pub first: AbsorptionReplayReport,
    pub second: AbsorptionReplayReport,
    pub deterministic: bool,
}

#[derive(Debug)]
pub struct AbsorptionDetector {
    cfg: AbsorptionConfig,
    orders: HashMap<u64, OrderState>,
    levels: HashMap<LevelKey, LevelState>,
    last_ts_ns: u64,
}

impl AbsorptionDetector {
    pub fn new(cfg: AbsorptionConfig) -> Self {
        Self {
            cfg,
            orders: HashMap::new(),
            levels: HashMap::new(),
            last_ts_ns: 0,
        }
    }

    pub fn config(&self) -> AbsorptionConfig {
        self.cfg
    }

    pub fn observe_obo(
        &mut self,
        ts_ns: u64,
        instrument_id: u64,
        event: OboEventV1,
    ) -> Option<AbsorptionSignal> {
        let ts_ns = self.monotonic_ts(ts_ns);
        match event {
            OboEventV1::Add(add) => {
                let side = side_from_u8(add.side)?;
                let replaced_existing = self.replace_order_without_behavior(add.order_id, ts_ns);
                let order = OrderState {
                    instrument_id,
                    price: add.price_e8,
                    qty: add.qty,
                    side,
                };
                self.orders.insert(add.order_id, order);
                self.update_level(
                    order.key(),
                    ts_ns,
                    add.qty as i128,
                    (!replaced_existing).then_some((ObservationKind::Replenish, add.qty)),
                )
            }
            OboEventV1::Modify(modify) => {
                let order_id = modify.order_id;
                let new_price_e8 = modify.new_price_e8;
                let new_qty = modify.new_qty;
                let flags = modify.flags;
                let mut order = *self.orders.get(&order_id)?;
                let new_price = if flags & 1 == 1 {
                    order.price
                } else {
                    new_price_e8
                };
                let old_key = order.key();
                let new_key = LevelKey {
                    instrument_id: order.instrument_id,
                    price: new_price,
                    side: order.side,
                };
                let old_qty = order.qty;
                order.price = new_price;
                order.qty = new_qty;
                if new_qty == 0 {
                    self.orders.remove(&order_id);
                } else {
                    self.orders.insert(order_id, order);
                }

                if old_key == new_key {
                    match new_qty.cmp(&old_qty) {
                        std::cmp::Ordering::Greater => {
                            let added = new_qty - old_qty;
                            self.update_level(
                                old_key,
                                ts_ns,
                                added as i128,
                                Some((ObservationKind::Replenish, added)),
                            )
                        }
                        std::cmp::Ordering::Less => {
                            let removed = old_qty - new_qty;
                            self.update_level(
                                old_key,
                                ts_ns,
                                -(removed as i128),
                                Some((ObservationKind::Pull, removed)),
                            )
                        }
                        std::cmp::Ordering::Equal => None,
                    }
                } else {
                    let pull = self.update_level(
                        old_key,
                        ts_ns,
                        -(old_qty as i128),
                        Some((ObservationKind::Pull, old_qty)),
                    );
                    let replenish = if new_qty == 0 {
                        None
                    } else {
                        self.update_level(
                            new_key,
                            ts_ns,
                            new_qty as i128,
                            Some((ObservationKind::Replenish, new_qty)),
                        )
                    };
                    pull.or(replenish)
                }
            }
            OboEventV1::Cancel(cancel) => {
                let order_id = cancel.order_id;
                let qty_cxl = cancel.qty_cxl;
                let mut order = *self.orders.get(&order_id)?;
                let removed = if qty_cxl == 0 {
                    order.qty
                } else {
                    qty_cxl.min(order.qty)
                };
                if removed == 0 {
                    return None;
                }
                order.qty -= removed;
                if order.qty == 0 {
                    self.orders.remove(&order_id);
                } else {
                    self.orders.insert(order_id, order);
                }
                self.update_level(
                    order.key(),
                    ts_ns,
                    -(removed as i128),
                    Some((ObservationKind::Pull, removed)),
                )
            }
            OboEventV1::Execute(exec) => {
                let maker_order_id = exec.maker_order_id;
                let trade_qty = exec.trade_qty;
                let trade_price_e8 = exec.trade_price_e8;
                let aggressor_side = side_from_u8(exec.aggressor_side)?;
                let passive_side = opposite_side(aggressor_side);
                let (key, visible_delta) =
                    if let Some(mut order) = self.orders.get(&maker_order_id).copied() {
                        let key = order.key();
                        let visible_removed = trade_qty.min(order.qty);
                        order.qty -= visible_removed;
                        if order.qty == 0 {
                            self.orders.remove(&maker_order_id);
                        } else {
                            self.orders.insert(maker_order_id, order);
                        }
                        (key, -(visible_removed as i128))
                    } else {
                        (
                            LevelKey {
                                instrument_id,
                                price: trade_price_e8,
                                side: passive_side,
                            },
                            0,
                        )
                    };
                self.update_level(
                    key,
                    ts_ns,
                    visible_delta,
                    Some((ObservationKind::Execute, trade_qty)),
                )
            }
        }
    }

    pub fn observe_raw_frame(
        &mut self,
        frame: &[u8],
    ) -> Result<Option<AbsorptionSignal>, OboFrameError> {
        let Some(parsed) = parse_obo_frame(frame)? else {
            return Ok(None);
        };
        Ok(self.observe_obo(parsed.send_time_ns, parsed.instrument_id, parsed.event))
    }

    fn monotonic_ts(&mut self, ts_ns: u64) -> u64 {
        let ts_ns = ts_ns.max(self.last_ts_ns);
        self.last_ts_ns = ts_ns;
        ts_ns
    }

    fn replace_order_without_behavior(&mut self, order_id: u64, ts_ns: u64) -> bool {
        let Some(old) = self.orders.remove(&order_id) else {
            return false;
        };
        let _ = self.update_level(old.key(), ts_ns, -(old.qty as i128), None);
        true
    }

    fn update_level(
        &mut self,
        key: LevelKey,
        ts_ns: u64,
        visible_delta: i128,
        observation: Option<(ObservationKind, u64)>,
    ) -> Option<AbsorptionSignal> {
        let level = self.levels.entry(key).or_default();
        level.evict(ts_ns, self.cfg.window_ns);

        let record_observation = match observation {
            Some((ObservationKind::Execute, qty)) => qty > 0,
            Some((_kind, qty)) => qty > 0 && level.has_execution_pressure(),
            None => false,
        };

        if visible_delta >= 0 {
            level.visible_qty = level.visible_qty.saturating_add(visible_delta as u64);
        } else {
            level.visible_qty = level.visible_qty.saturating_sub((-visible_delta) as u64);
        }

        if !record_observation {
            return None;
        }

        let (kind, qty) = observation?;
        level
            .observations
            .push_back(LevelObservation { ts_ns, qty, kind });
        level.maybe_signal(key, ts_ns, &self.cfg)
    }
}

pub fn parse_obo_frame(frame: &[u8]) -> Result<Option<ParsedOboEvent>, OboFrameError> {
    if frame.len() < FRAME_HEADER_LEN {
        return Err(OboFrameError::ShortHeader);
    }
    if frame[0..4] != codec_raw::MAGIC {
        return Err(OboFrameError::InvalidMagic);
    }
    let version = frame[4];
    if version != codec_raw::VERSION_V1 {
        return Err(OboFrameError::UnsupportedVersion(version));
    }
    let codec = frame[5];
    if codec != codec_raw::codec::RAW_V1 {
        return Err(OboFrameError::UnsupportedCodec(codec));
    }
    let message_type = le_u16(&frame[6..8]);
    let channel = le_u32(&frame[8..12]);
    if channel != channel_id::OBO_L3 {
        return Err(OboFrameError::UnsupportedChannel(channel));
    }
    let instrument_id = le_u64(&frame[12..20]);
    let sequence = le_u64(&frame[20..28]);
    let global_sequence = le_u64(&frame[28..36]);
    let send_time_ns = le_u64(&frame[36..44]);
    let payload_len = le_u32(&frame[44..48]) as usize;
    let actual_payload_len = frame.len().saturating_sub(FRAME_HEADER_LEN);
    if actual_payload_len != payload_len {
        return Err(OboFrameError::PayloadLengthMismatch {
            expected: payload_len,
            actual: actual_payload_len,
        });
    }
    let payload = &frame[FRAME_HEADER_LEN..];
    let event = match message_type {
        msg_type::HEARTBEAT
        | msg_type::GAP
        | msg_type::SNAPSHOT_START
        | msg_type::SNAPSHOT_END
        | msg_type::SNAPSHOT_HDR => return Ok(None),
        msg_type::OBO_ADD => {
            require_payload_len(message_type, payload, std::mem::size_of::<OboAddV1>())?;
            OboEventV1::Add(OboAddV1 {
                order_id: le_u64(&payload[0..8]),
                price_e8: le_i64(&payload[8..16]),
                qty: le_u64(&payload[16..24]),
                side: payload[24],
                flags: payload[25],
            })
        }
        msg_type::OBO_MODIFY => {
            require_payload_len(message_type, payload, std::mem::size_of::<OboModifyV1>())?;
            OboEventV1::Modify(OboModifyV1 {
                order_id: le_u64(&payload[0..8]),
                new_price_e8: le_i64(&payload[8..16]),
                new_qty: le_u64(&payload[16..24]),
                flags: payload[24],
            })
        }
        msg_type::OBO_CANCEL => {
            require_payload_len(message_type, payload, std::mem::size_of::<OboCancelV1>())?;
            OboEventV1::Cancel(OboCancelV1 {
                order_id: le_u64(&payload[0..8]),
                qty_cxl: le_u64(&payload[8..16]),
                reason: payload[16],
            })
        }
        msg_type::OBO_EXECUTE => {
            require_payload_len(message_type, payload, std::mem::size_of::<OboExecuteV1>())?;
            OboEventV1::Execute(OboExecuteV1 {
                maker_order_id: le_u64(&payload[0..8]),
                trade_qty: le_u64(&payload[8..16]),
                trade_price_e8: le_i64(&payload[16..24]),
                aggressor_side: payload[24],
                match_id: le_u64(&payload[25..33]),
            })
        }
        other => return Err(OboFrameError::UnsupportedMessageType(other)),
    };
    Ok(Some(ParsedOboEvent {
        instrument_id,
        sequence,
        global_sequence,
        send_time_ns,
        event,
    }))
}

pub fn replay_absorption_frames<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: AbsorptionConfig,
) -> AbsorptionReplayReport {
    let mut detector = AbsorptionDetector::new(cfg);
    let mut dedupe = OboLiveDedupe::new();
    let mut report = AbsorptionReplayReport::default();
    for frame in frames {
        report.frames_total = report.frames_total.saturating_add(1);
        match parse_obo_frame(frame.as_ref()) {
            Ok(Some(parsed)) => {
                if !dedupe.accept(&parsed) {
                    report.duplicate_events = report.duplicate_events.saturating_add(1);
                    continue;
                }
                report.parsed_events = report.parsed_events.saturating_add(1);
                if let Some(signal) =
                    detector.observe_obo(parsed.send_time_ns, parsed.instrument_id, parsed.event)
                {
                    report.record_signal(&signal);
                }
            }
            Ok(None) => {
                report.control_frames = report.control_frames.saturating_add(1);
            }
            Err(_err) => {
                report.parse_errors = report.parse_errors.saturating_add(1);
            }
        }
    }
    report
}

pub fn validate_absorption_replay<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: AbsorptionConfig,
) -> AbsorptionReplayValidation {
    let first = replay_absorption_frames(frames, cfg);
    let second = replay_absorption_frames(frames, cfg);
    let deterministic = first == second;
    AbsorptionReplayValidation {
        first,
        second,
        deterministic,
    }
}

impl AbsorptionReplayReport {
    fn record_signal(&mut self, signal: &AbsorptionSignal) {
        self.signals = self.signals.saturating_add(1);
        self.first_signal_ns.get_or_insert(signal.window_end_ns);
        self.last_signal_ns = Some(signal.window_end_ns);
        hash_signal(&mut self.signal_hash, signal);
    }
}

impl OrderState {
    fn key(self) -> LevelKey {
        LevelKey {
            instrument_id: self.instrument_id,
            price: self.price,
            side: self.side,
        }
    }
}

impl LevelState {
    fn evict(&mut self, now_ns: u64, window_ns: u64) {
        let cutoff = now_ns.saturating_sub(window_ns);
        while let Some(front) = self.observations.front() {
            if front.ts_ns >= cutoff {
                break;
            }
            self.observations.pop_front();
        }
    }

    fn has_execution_pressure(&self) -> bool {
        self.observations
            .iter()
            .any(|obs| obs.kind == ObservationKind::Execute)
    }

    fn maybe_signal(
        &mut self,
        key: LevelKey,
        now_ns: u64,
        cfg: &AbsorptionConfig,
    ) -> Option<AbsorptionSignal> {
        if self
            .last_signal_ns
            .is_some_and(|last| now_ns.saturating_sub(last) < cfg.cooldown_ns)
        {
            return None;
        }

        let mut window_start_ns = now_ns;
        let mut executed_qty = 0_u64;
        let mut replenished_qty = 0_u64;
        let mut pulled_qty = 0_u64;
        let mut execute_events = 0_u32;
        let mut replenish_events = 0_u32;
        let mut pull_events = 0_u32;

        for obs in &self.observations {
            window_start_ns = window_start_ns.min(obs.ts_ns);
            match obs.kind {
                ObservationKind::Execute => {
                    executed_qty = executed_qty.saturating_add(obs.qty);
                    execute_events = execute_events.saturating_add(1);
                }
                ObservationKind::Replenish => {
                    replenished_qty = replenished_qty.saturating_add(obs.qty);
                    replenish_events = replenish_events.saturating_add(1);
                }
                ObservationKind::Pull => {
                    pulled_qty = pulled_qty.saturating_add(obs.qty);
                    pull_events = pull_events.saturating_add(1);
                }
            }
        }

        if executed_qty < cfg.min_executed_qty || execute_events < cfg.min_execute_events {
            return None;
        }

        let replenishment_ratio_bps = ratio_bps(replenished_qty, executed_qty);
        let pull_ratio_bps = ratio_bps(pulled_qty, executed_qty);
        let enough_replenishment = replenished_qty >= cfg.min_replenished_qty
            && replenishment_ratio_bps >= cfg.min_replenishment_ratio_bps;
        let level_held = self.visible_qty >= cfg.min_visible_qty_after;
        if !enough_replenishment && !level_held {
            return None;
        }
        if pull_ratio_bps > cfg.max_pull_ratio_bps {
            return None;
        }

        let confidence_bps = confidence_bps(
            executed_qty,
            execute_events,
            replenishment_ratio_bps,
            pull_ratio_bps,
            self.visible_qty,
            cfg,
        );
        self.last_signal_ns = Some(now_ns);
        Some(AbsorptionSignal {
            instrument_id: key.instrument_id,
            price: key.price,
            passive_side: key.side,
            aggressor_side: opposite_side(key.side),
            window_start_ns,
            window_end_ns: now_ns,
            executed_qty,
            replenished_qty,
            pulled_qty,
            visible_qty_after: self.visible_qty,
            execute_events,
            replenish_events,
            pull_events,
            replenishment_ratio_bps,
            pull_ratio_bps,
            confidence_bps,
        })
    }
}

fn confidence_bps(
    executed_qty: u64,
    execute_events: u32,
    replenishment_ratio_bps: u32,
    pull_ratio_bps: u32,
    visible_qty_after: u64,
    cfg: &AbsorptionConfig,
) -> u16 {
    let pressure_score = ratio_bps(executed_qty, cfg.min_executed_qty).min(10_000);
    let event_score =
        ratio_bps(u64::from(execute_events), u64::from(cfg.min_execute_events)).min(10_000);
    let hold_score = ratio_bps(
        visible_qty_after,
        visible_qty_after.saturating_add(executed_qty),
    )
    .min(10_000);
    let passive_score = replenishment_ratio_bps.min(10_000).max(hold_score);
    let pull_score = 10_000_u32.saturating_sub(pull_ratio_bps.min(10_000));
    let weighted = pressure_score * 30 + event_score * 20 + passive_score * 35 + pull_score * 15;
    (weighted / 100).min(10_000) as u16
}

fn ratio_bps(numerator: u64, denominator: u64) -> u32 {
    if denominator == 0 {
        return 0;
    }
    (((numerator as u128) * 10_000) / (denominator as u128)).min(u128::from(u32::MAX)) as u32
}

fn hash_signal(hash: &mut u64, signal: &AbsorptionSignal) {
    hash_u64(hash, signal.instrument_id);
    hash_i64(hash, signal.price);
    hash_u8(hash, side_to_u8(signal.passive_side));
    hash_u8(hash, side_to_u8(signal.aggressor_side));
    hash_u64(hash, signal.window_start_ns);
    hash_u64(hash, signal.window_end_ns);
    hash_u64(hash, signal.executed_qty);
    hash_u64(hash, signal.replenished_qty);
    hash_u64(hash, signal.pulled_qty);
    hash_u64(hash, signal.visible_qty_after);
    hash_u32(hash, signal.execute_events);
    hash_u32(hash, signal.replenish_events);
    hash_u32(hash, signal.pull_events);
    hash_u32(hash, signal.replenishment_ratio_bps);
    hash_u32(hash, signal.pull_ratio_bps);
    hash_u16(hash, signal.confidence_bps);
}

fn hash_u8(hash: &mut u64, value: u8) {
    *hash ^= u64::from(value);
    *hash = hash.wrapping_mul(REPLAY_HASH_PRIME);
}

fn hash_u16(hash: &mut u64, value: u16) {
    for byte in value.to_le_bytes() {
        hash_u8(hash, byte);
    }
}

fn hash_u32(hash: &mut u64, value: u32) {
    for byte in value.to_le_bytes() {
        hash_u8(hash, byte);
    }
}

fn hash_u64(hash: &mut u64, value: u64) {
    for byte in value.to_le_bytes() {
        hash_u8(hash, byte);
    }
}

fn hash_i64(hash: &mut u64, value: i64) {
    hash_u64(hash, value as u64);
}

fn require_payload_len(
    message_type: u16,
    payload: &[u8],
    expected: usize,
) -> Result<(), OboFrameError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(OboFrameError::InvalidPayloadLength {
            message_type,
            expected,
            actual: payload.len(),
        })
    }
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn le_i64(bytes: &[u8]) -> i64 {
    i64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn side_from_u8(side: u8) -> Option<Side> {
    match side {
        0 => Some(Side::Bid),
        1 => Some(Side::Ask),
        _ => None,
    }
}

fn side_to_u8(side: Side) -> u8 {
    match side {
        Side::Bid => 0,
        Side::Ask => 1,
    }
}

fn opposite_side(side: Side) -> Side {
    match side {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec_raw::{self, FrameHeaderV1, OboAddV1, OboCancelV1, OboExecuteV1, OboModifyV1};
    use zerocopy::AsBytes;

    fn cfg() -> AbsorptionConfig {
        AbsorptionConfig {
            window_ns: 1_000,
            min_executed_qty: 100,
            min_execute_events: 2,
            min_replenished_qty: 50,
            min_replenishment_ratio_bps: 5_000,
            min_visible_qty_after: 25,
            max_pull_ratio_bps: 2_500,
            cooldown_ns: 500,
        }
    }

    fn add(order_id: u64, price: i64, qty: u64, side: Side) -> OboEventV1 {
        OboEventV1::Add(OboAddV1 {
            order_id,
            price_e8: price,
            qty,
            side: side_to_u8(side),
            flags: 0,
        })
    }

    fn qty_modify(order_id: u64, qty: u64) -> OboEventV1 {
        OboEventV1::Modify(OboModifyV1 {
            order_id,
            new_price_e8: 0,
            new_qty: qty,
            flags: 1,
        })
    }

    fn cancel(order_id: u64, qty_cxl: u64) -> OboEventV1 {
        OboEventV1::Cancel(OboCancelV1 {
            order_id,
            qty_cxl,
            reason: 0,
        })
    }

    fn execute(maker_order_id: u64, price: i64, qty: u64, aggressor_side: Side) -> OboEventV1 {
        OboEventV1::Execute(OboExecuteV1 {
            maker_order_id,
            trade_qty: qty,
            trade_price_e8: price,
            aggressor_side: side_to_u8(aggressor_side),
            match_id: 1,
        })
    }

    fn raw_frame(
        message_type: u16,
        instrument_id: u64,
        sequence: u64,
        global_sequence: u64,
        send_time_ns: u64,
        payload: &[u8],
    ) -> Vec<u8> {
        let header = FrameHeaderV1 {
            magic: codec_raw::MAGIC,
            version: codec_raw::VERSION_V1,
            codec: codec_raw::codec::RAW_V1,
            message_type,
            channel_id: channel_id::OBO_L3,
            instrument_id,
            sequence,
            global_sequence,
            send_time_ns,
            payload_len: payload.len() as u32,
        };
        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        frame.extend_from_slice(header.as_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn event_payload(event: &OboEventV1) -> &[u8] {
        match event {
            OboEventV1::Add(payload) => payload.as_bytes(),
            OboEventV1::Modify(payload) => payload.as_bytes(),
            OboEventV1::Cancel(payload) => payload.as_bytes(),
            OboEventV1::Execute(payload) => payload.as_bytes(),
        }
    }

    #[test]
    fn detects_bid_absorption_with_replenishment() {
        let mut detector = AbsorptionDetector::new(cfg());
        assert!(detector
            .observe_obo(10, 7, add(1, 100, 100, Side::Bid))
            .is_none());
        assert!(detector
            .observe_obo(20, 7, execute(1, 100, 70, Side::Ask))
            .is_none());
        assert!(detector
            .observe_obo(30, 7, add(2, 100, 80, Side::Bid))
            .is_none());

        let signal = detector
            .observe_obo(40, 7, execute(2, 100, 30, Side::Ask))
            .expect("absorption signal");
        assert_eq!(signal.instrument_id, 7);
        assert_eq!(signal.price, 100);
        assert_eq!(signal.passive_side, Side::Bid);
        assert_eq!(signal.aggressor_side, Side::Ask);
        assert_eq!(signal.executed_qty, 100);
        assert_eq!(signal.replenished_qty, 80);
        assert_eq!(signal.pulled_qty, 0);
        assert_eq!(signal.visible_qty_after, 80);
        assert_eq!(signal.execute_events, 2);
        assert_eq!(signal.replenishment_ratio_bps, 8_000);
        assert!(signal.confidence_bps >= 8_000);
    }

    #[test]
    fn rejects_pull_after_pressure_as_absorption() {
        let mut config = cfg();
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 200, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 50, Side::Ask));
        detector.observe_obo(30, 7, execute(1, 100, 50, Side::Ask));
        let signal = detector.observe_obo(40, 7, cancel(1, 100));
        assert!(signal.is_none());
    }

    #[test]
    fn ignores_replenishment_before_execution_pressure() {
        let mut config = cfg();
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, add(2, 100, 100, Side::Bid));
        detector.observe_obo(30, 7, execute(1, 100, 50, Side::Ask));
        let signal = detector.observe_obo(40, 7, execute(2, 100, 50, Side::Ask));
        assert!(signal.is_none());
    }

    #[test]
    fn qty_increase_modify_after_pressure_counts_as_replenishment() {
        let mut config = cfg();
        config.min_executed_qty = 80;
        config.min_execute_events = 1;
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 80, Side::Ask));
        let signal = detector
            .observe_obo(30, 7, qty_modify(1, 70))
            .expect("modify replenish signal");
        assert_eq!(signal.executed_qty, 80);
        assert_eq!(signal.replenished_qty, 50);
        assert_eq!(signal.visible_qty_after, 70);
        assert_eq!(signal.replenish_events, 1);
    }

    #[test]
    fn cooldown_suppresses_repeated_signals_on_same_level() {
        let mut detector = AbsorptionDetector::new(cfg());
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 70, Side::Ask));
        detector.observe_obo(30, 7, add(2, 100, 80, Side::Bid));
        assert!(detector
            .observe_obo(40, 7, execute(2, 100, 30, Side::Ask))
            .is_some());

        detector.observe_obo(50, 7, add(3, 100, 100, Side::Bid));
        let repeated = detector.observe_obo(60, 7, execute(3, 100, 50, Side::Ask));
        assert!(repeated.is_none());
    }

    #[test]
    fn unknown_maker_execute_can_still_seed_pressure_by_price() {
        let mut config = cfg();
        config.min_executed_qty = 80;
        config.min_execute_events = 1;
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        detector.observe_obo(10, 7, execute(99, 100, 80, Side::Ask));
        let signal = detector
            .observe_obo(20, 7, add(1, 100, 60, Side::Bid))
            .expect("price-level absorption signal");
        assert_eq!(signal.passive_side, Side::Bid);
        assert_eq!(signal.aggressor_side, Side::Ask);
        assert_eq!(signal.executed_qty, 80);
        assert_eq!(signal.replenished_qty, 60);
    }

    #[test]
    fn duplicate_add_replacement_does_not_count_as_replenishment() {
        let mut config = cfg();
        config.min_executed_qty = 80;
        config.min_execute_events = 1;
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 80, Side::Ask));
        let signal = detector.observe_obo(30, 7, add(1, 100, 100, Side::Bid));
        assert!(signal.is_none());
    }

    #[test]
    fn raw_frame_parser_feeds_absorption_detector() {
        let mut config = cfg();
        config.min_executed_qty = 80;
        config.min_execute_events = 1;
        config.min_visible_qty_after = 1_000;
        let mut detector = AbsorptionDetector::new(config);
        let add_event = add(1, 100, 100, Side::Bid);
        let execute_event = execute(1, 100, 80, Side::Ask);
        let replenish_event = add(2, 100, 60, Side::Bid);

        let add_frame = raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_event));
        assert!(detector.observe_raw_frame(&add_frame).unwrap().is_none());
        let execute_frame = raw_frame(
            msg_type::OBO_EXECUTE,
            7,
            2,
            11,
            110,
            event_payload(&execute_event),
        );
        assert!(detector
            .observe_raw_frame(&execute_frame)
            .unwrap()
            .is_none());
        let replenish_frame = raw_frame(
            msg_type::OBO_ADD,
            7,
            3,
            12,
            120,
            event_payload(&replenish_event),
        );
        let signal = detector
            .observe_raw_frame(&replenish_frame)
            .unwrap()
            .expect("raw frame absorption signal");
        assert_eq!(signal.instrument_id, 7);
        assert_eq!(signal.price, 100);
        assert_eq!(signal.window_start_ns, 110);
        assert_eq!(signal.window_end_ns, 120);
        assert_eq!(signal.replenished_qty, 60);
    }

    #[test]
    fn raw_frame_parser_rejects_payload_length_mismatch() {
        let add = add(1, 100, 100, Side::Bid);
        let mut frame = raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add));
        frame.push(0);
        assert!(matches!(
            parse_obo_frame(&frame),
            Err(OboFrameError::PayloadLengthMismatch { .. })
        ));
    }

    #[test]
    fn replay_validation_is_deterministic_and_dedupes_live_frames() {
        let mut config = cfg();
        config.min_executed_qty = 80;
        config.min_execute_events = 1;
        config.min_visible_qty_after = 1_000;
        let add_event = add(1, 100, 100, Side::Bid);
        let execute_event = execute(1, 100, 80, Side::Ask);
        let replenish_event = add(2, 100, 60, Side::Bid);
        let frames = vec![
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_event)),
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_event)),
            raw_frame(
                msg_type::OBO_EXECUTE,
                7,
                2,
                11,
                110,
                event_payload(&execute_event),
            ),
            raw_frame(
                msg_type::OBO_ADD,
                7,
                3,
                12,
                120,
                event_payload(&replenish_event),
            ),
        ];

        let validation = validate_absorption_replay(&frames, config);
        assert!(validation.deterministic);
        assert_eq!(validation.first.frames_total, 4);
        assert_eq!(validation.first.parsed_events, 3);
        assert_eq!(validation.first.duplicate_events, 1);
        assert_eq!(validation.first.signals, 1);
        assert_ne!(validation.first.signal_hash, REPLAY_HASH_OFFSET);
        assert_eq!(validation.first, validation.second);
    }
}
