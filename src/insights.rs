use crate::codec_raw::{
    self, channel_id, msg_type, OboAddV1, OboCancelV1, OboExecuteV1, OboModifyV1,
};
use crate::obo::OboEventV1;
use crate::parser::Side;
use chrono::{Datelike, Utc};
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::hash::{Hash, Hasher};

const FRAME_HEADER_LEN: usize = 48;
const REPLAY_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const REPLAY_HASH_PRIME: u64 = 0x00000100000001b3;
const NS_PER_SECOND: u64 = 1_000_000_000;
const FEATURE_15S_NS: u64 = 15 * NS_PER_SECOND;
const FEATURE_60S_NS: u64 = 60 * NS_PER_SECOND;
const FEATURE_120S_NS: u64 = 120 * NS_PER_SECOND;
const FEATURE_300S_NS: u64 = 300 * NS_PER_SECOND;
const DEFAULT_FEATURE_DEPTH_LEVELS: usize = 10;
const MAX_FEATURE_DEPTH_LEVELS: usize = 64;
const OUTCOME_HORIZONS_NS: [u64; 3] = [NS_PER_SECOND, 5 * NS_PER_SECOND, 30 * NS_PER_SECOND];
const DEFAULT_OUTCOME_MAX_PENDING: usize = 32_768;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IcebergObservation {
    ts_ns: u64,
    qty: u64,
    kind: ObservationKind,
    visible_before: u64,
    visible_after: u64,
}

#[derive(Debug, Default)]
struct LevelState {
    visible_qty: u64,
    observations: VecDeque<LevelObservation>,
    last_signal_ns: Option<u64>,
}

#[derive(Debug, Default)]
struct IcebergLevelState {
    visible_qty: u64,
    observations: VecDeque<IcebergObservation>,
    last_signal_ns: Option<u64>,
}

#[derive(Debug, Default)]
struct LiquidityPullLevelState {
    visible_qty: u64,
    observations: VecDeque<IcebergObservation>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcebergConfig {
    pub window_ns: u64,
    pub min_executed_qty: u64,
    pub min_execute_events: u32,
    pub min_replenish_events: u32,
    pub min_replenished_qty: u64,
    pub min_replenishment_ratio_bps: u32,
    pub min_over_display_ratio_bps: u32,
    pub max_pull_ratio_bps: u32,
    pub cooldown_ns: u64,
}

impl Default for IcebergConfig {
    fn default() -> Self {
        Self {
            window_ns: 5_000_000_000,
            min_executed_qty: 100,
            min_execute_events: 3,
            min_replenish_events: 2,
            min_replenished_qty: 75,
            min_replenishment_ratio_bps: 5_000,
            min_over_display_ratio_bps: 12_500,
            max_pull_ratio_bps: 2_500,
            cooldown_ns: 2_000_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcebergSignal {
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
    pub max_visible_qty: u64,
    pub execute_events: u32,
    pub replenish_events: u32,
    pub pull_events: u32,
    pub replenishment_ratio_bps: u32,
    pub over_display_ratio_bps: u32,
    pub pull_ratio_bps: u32,
    pub confidence_bps: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidityPullConfig {
    pub window_ns: u64,
    pub min_pulled_qty: u64,
    pub min_pull_events: u32,
    pub min_visible_qty: u64,
    pub min_pull_ratio_bps: u32,
    pub max_execution_ratio_bps: u32,
    pub max_visible_after_ratio_bps: u32,
    pub cooldown_ns: u64,
}

impl Default for LiquidityPullConfig {
    fn default() -> Self {
        Self {
            window_ns: 1_000_000_000,
            min_pulled_qty: 100,
            min_pull_events: 2,
            min_visible_qty: 100,
            min_pull_ratio_bps: 5_000,
            max_execution_ratio_bps: 2_500,
            max_visible_after_ratio_bps: 5_000,
            cooldown_ns: 1_000_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidityPullSignal {
    pub instrument_id: u64,
    pub price: i64,
    pub pulled_side: Side,
    pub opposing_side: Side,
    pub window_start_ns: u64,
    pub window_end_ns: u64,
    pub pulled_qty: u64,
    pub executed_qty: u64,
    pub replenished_qty: u64,
    pub visible_qty_after: u64,
    pub max_visible_qty: u64,
    pub pull_events: u32,
    pub execute_events: u32,
    pub replenish_events: u32,
    pub pull_ratio_bps: u32,
    pub execution_ratio_bps: u32,
    pub visible_after_ratio_bps: u32,
    pub confidence_bps: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Absorption,
    Iceberg,
    LiquidityPull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalRegime {
    Balance,
    Absorption,
    SpoofRisk,
    InitiativeFlow,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignalCounts {
    pub total: u64,
    pub absorption: u64,
    pub iceberg: u64,
    pub liquidity_pull: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignalScoreSummary {
    pub signals: u64,
    pub max_score_bps: u16,
    pub avg_score_bps: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalFeatureSummary {
    pub signal_kind: SignalKind,
    pub feature: String,
    pub observations: u64,
    pub weight_pct: u8,
    pub avg_score_bps: u16,
    pub avg_contribution_bps: u16,
    pub total_contribution_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalDiagnostics {
    pub regime: SignalRegime,
    pub counts: SignalCounts,
    pub score: SignalScoreSummary,
    pub absorption_score: SignalScoreSummary,
    pub iceberg_score: SignalScoreSummary,
    pub liquidity_pull_score: SignalScoreSummary,
    pub first_signal_ns: Option<u64>,
    pub last_signal_ns: Option<u64>,
    pub top_features: Vec<SignalFeatureSummary>,
}

impl Default for SignalDiagnostics {
    fn default() -> Self {
        Self {
            regime: SignalRegime::Balance,
            counts: SignalCounts::default(),
            score: SignalScoreSummary::default(),
            absorption_score: SignalScoreSummary::default(),
            iceberg_score: SignalScoreSummary::default(),
            liquidity_pull_score: SignalScoreSummary::default(),
            first_signal_ns: None,
            last_signal_ns: None,
            top_features: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SignalDiagnosticsAccumulator {
    counts: SignalCounts,
    score: ScoreAccumulator,
    absorption_score: ScoreAccumulator,
    iceberg_score: ScoreAccumulator,
    liquidity_pull_score: ScoreAccumulator,
    first_signal_ns: Option<u64>,
    last_signal_ns: Option<u64>,
    feature_totals: HashMap<SignalFeatureKey, FeatureScoreAccumulator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SignalFeatureKey {
    signal_kind: SignalKind,
    feature: &'static str,
}

#[derive(Debug, Clone, Copy, Default)]
struct ScoreAccumulator {
    signals: u64,
    score_sum_bps: u64,
    max_score_bps: u16,
}

#[derive(Debug, Clone, Copy, Default)]
struct FeatureScoreAccumulator {
    observations: u64,
    score_sum_bps: u64,
    contribution_sum_bps: u64,
    weight_pct: u8,
}

#[derive(Debug, Clone, Copy)]
struct SignalFeatureContribution {
    signal_kind: SignalKind,
    feature: &'static str,
    score_bps: u32,
    weight_pct: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "signal", rename_all = "snake_case")]
pub enum ParticipantSignal {
    Absorption(AbsorptionSignal),
    Iceberg(IcebergSignal),
    LiquidityPull(LiquidityPullSignal),
}

impl ParticipantSignal {
    pub fn instrument_id(&self) -> u64 {
        match self {
            ParticipantSignal::Absorption(signal) => signal.instrument_id,
            ParticipantSignal::Iceberg(signal) => signal.instrument_id,
            ParticipantSignal::LiquidityPull(signal) => signal.instrument_id,
        }
    }

    pub fn window_end_ns(&self) -> u64 {
        match self {
            ParticipantSignal::Absorption(signal) => signal.window_end_ns,
            ParticipantSignal::Iceberg(signal) => signal.window_end_ns,
            ParticipantSignal::LiquidityPull(signal) => signal.window_end_ns,
        }
    }

    pub fn kind(&self) -> SignalKind {
        match self {
            ParticipantSignal::Absorption(_) => SignalKind::Absorption,
            ParticipantSignal::Iceberg(_) => SignalKind::Iceberg,
            ParticipantSignal::LiquidityPull(_) => SignalKind::LiquidityPull,
        }
    }

    pub fn confidence_bps(&self) -> u16 {
        match self {
            ParticipantSignal::Absorption(signal) => signal.confidence_bps,
            ParticipantSignal::Iceberg(signal) => signal.confidence_bps,
            ParticipantSignal::LiquidityPull(signal) => signal.confidence_bps,
        }
    }

    pub fn expected_direction(&self) -> i8 {
        match self {
            ParticipantSignal::Absorption(signal) => side_direction(signal.passive_side),
            ParticipantSignal::Iceberg(signal) => side_direction(signal.passive_side),
            ParticipantSignal::LiquidityPull(signal) => -side_direction(signal.pulled_side),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SignalOutcomeSummary {
    pub tracked_signals: u64,
    pub pending_signals: usize,
    pub dropped_pending: u64,
    pub rows: Vec<SignalOutcomeRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalOutcomeRow {
    pub signal_kind: SignalKind,
    pub horizon_ns: u64,
    pub observations: u64,
    pub favorable: u64,
    pub adverse: u64,
    pub flat: u64,
    pub avg_signed_markout_e8: i64,
    pub avg_abs_markout_e8: u64,
    pub max_favorable_markout_e8: i64,
    pub max_adverse_markout_e8: i64,
}

#[derive(Debug, Clone)]
pub struct SignalOutcomeTracker {
    max_pending: usize,
    tracked_signals: u64,
    dropped_pending: u64,
    pending: VecDeque<PendingSignalOutcome>,
    totals: HashMap<SignalOutcomeKey, SignalOutcomeAccumulator>,
}

#[derive(Debug, Clone, Copy)]
struct PendingSignalOutcome {
    signal_kind: SignalKind,
    instrument_id: u64,
    signal_ts_ns: u64,
    base_mid_price_e8: i64,
    direction: i8,
    settled: [bool; OUTCOME_HORIZONS_NS.len()],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SignalOutcomeKey {
    signal_kind: SignalKind,
    horizon_ns: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct SignalOutcomeAccumulator {
    observations: u64,
    favorable: u64,
    adverse: u64,
    flat: u64,
    signed_sum_e8: i128,
    abs_sum_e8: u128,
    max_favorable_markout_e8: i64,
    max_adverse_markout_e8: i64,
}

impl Default for SignalOutcomeTracker {
    fn default() -> Self {
        Self::with_max_pending(DEFAULT_OUTCOME_MAX_PENDING)
    }
}

impl SignalOutcomeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_pending(max_pending: usize) -> Self {
        let max_pending = max_pending.max(1);
        Self {
            max_pending,
            tracked_signals: 0,
            dropped_pending: 0,
            pending: VecDeque::with_capacity(max_pending.min(1024)),
            totals: HashMap::new(),
        }
    }

    pub fn track_signal(
        &mut self,
        signal: &ParticipantSignal,
        snapshot: &MicrostructureFeatureSnapshot,
    ) -> bool {
        if snapshot.instrument_id != signal.instrument_id() {
            return false;
        }
        let Some(base_mid_price_e8) = snapshot.mid_price_e8 else {
            return false;
        };
        let direction = signal.expected_direction();
        if direction == 0 {
            return false;
        }
        while self.pending.len() >= self.max_pending {
            self.pending.pop_front();
            self.dropped_pending = self.dropped_pending.saturating_add(1);
        }
        self.pending.push_back(PendingSignalOutcome {
            signal_kind: signal.kind(),
            instrument_id: signal.instrument_id(),
            signal_ts_ns: signal.window_end_ns().max(snapshot.ts_ns),
            base_mid_price_e8,
            direction,
            settled: [false; OUTCOME_HORIZONS_NS.len()],
        });
        self.tracked_signals = self.tracked_signals.saturating_add(1);
        true
    }

    pub fn observe_snapshot(&mut self, snapshot: &MicrostructureFeatureSnapshot) {
        let Some(mid_price_e8) = snapshot.mid_price_e8 else {
            return;
        };
        if self.pending.is_empty() {
            return;
        }

        let mut retained = VecDeque::with_capacity(self.pending.len());
        while let Some(mut pending) = self.pending.pop_front() {
            if pending.instrument_id == snapshot.instrument_id {
                self.settle_pending(&mut pending, snapshot.ts_ns, mid_price_e8);
            }
            if !pending.settled.iter().all(|settled| *settled) {
                retained.push_back(pending);
            }
        }
        self.pending = retained;
    }

    pub fn snapshot(&self) -> SignalOutcomeSummary {
        let mut rows = self
            .totals
            .iter()
            .map(|(key, totals)| totals.row(*key))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.signal_kind
                .cmp(&right.signal_kind)
                .then_with(|| left.horizon_ns.cmp(&right.horizon_ns))
        });
        SignalOutcomeSummary {
            tracked_signals: self.tracked_signals,
            pending_signals: self.pending.len(),
            dropped_pending: self.dropped_pending,
            rows,
        }
    }

    fn settle_pending(
        &mut self,
        pending: &mut PendingSignalOutcome,
        snapshot_ts_ns: u64,
        mid_price_e8: i64,
    ) {
        for (idx, horizon_ns) in OUTCOME_HORIZONS_NS.iter().copied().enumerate() {
            if pending.settled[idx] {
                continue;
            }
            if snapshot_ts_ns < pending.signal_ts_ns.saturating_add(horizon_ns) {
                continue;
            }
            let markout = mid_price_e8.saturating_sub(pending.base_mid_price_e8);
            let signed_markout = markout.saturating_mul(i64::from(pending.direction));
            self.totals
                .entry(SignalOutcomeKey {
                    signal_kind: pending.signal_kind,
                    horizon_ns,
                })
                .or_default()
                .record(signed_markout);
            pending.settled[idx] = true;
        }
    }
}

impl SignalOutcomeAccumulator {
    fn record(&mut self, signed_markout_e8: i64) {
        self.observations = self.observations.saturating_add(1);
        self.signed_sum_e8 = self
            .signed_sum_e8
            .saturating_add(i128::from(signed_markout_e8));
        self.abs_sum_e8 = self
            .abs_sum_e8
            .saturating_add(u128::from(signed_markout_e8.unsigned_abs()));
        if signed_markout_e8 > 0 {
            self.favorable = self.favorable.saturating_add(1);
            self.max_favorable_markout_e8 = self.max_favorable_markout_e8.max(signed_markout_e8);
        } else if signed_markout_e8 < 0 {
            self.adverse = self.adverse.saturating_add(1);
            self.max_adverse_markout_e8 = self.max_adverse_markout_e8.min(signed_markout_e8);
        } else {
            self.flat = self.flat.saturating_add(1);
        }
    }

    fn row(&self, key: SignalOutcomeKey) -> SignalOutcomeRow {
        SignalOutcomeRow {
            signal_kind: key.signal_kind,
            horizon_ns: key.horizon_ns,
            observations: self.observations,
            favorable: self.favorable,
            adverse: self.adverse,
            flat: self.flat,
            avg_signed_markout_e8: avg_i128_to_i64(self.signed_sum_e8, self.observations),
            avg_abs_markout_e8: avg_u128_to_u64(self.abs_sum_e8, self.observations),
            max_favorable_markout_e8: self.max_favorable_markout_e8,
            max_adverse_markout_e8: self.max_adverse_markout_e8,
        }
    }
}

impl SignalDiagnosticsAccumulator {
    const TOP_FEATURE_LIMIT: usize = 8;

    pub fn record_signal(
        &mut self,
        signal: &ParticipantSignal,
        absorption_cfg: &AbsorptionConfig,
        iceberg_cfg: &IcebergConfig,
        liquidity_pull_cfg: &LiquidityPullConfig,
    ) {
        match signal {
            ParticipantSignal::Absorption(signal) => {
                self.record_absorption_signal(signal, absorption_cfg)
            }
            ParticipantSignal::Iceberg(signal) => self.record_iceberg_signal(signal, iceberg_cfg),
            ParticipantSignal::LiquidityPull(signal) => {
                self.record_liquidity_pull_signal(signal, liquidity_pull_cfg)
            }
        }
    }

    pub fn record_absorption_signal(&mut self, signal: &AbsorptionSignal, cfg: &AbsorptionConfig) {
        self.record_common(
            SignalKind::Absorption,
            signal.window_end_ns,
            signal.confidence_bps,
        );
        self.record_features(&absorption_feature_contributions(signal, cfg));
    }

    pub fn record_iceberg_signal(&mut self, signal: &IcebergSignal, cfg: &IcebergConfig) {
        self.record_common(
            SignalKind::Iceberg,
            signal.window_end_ns,
            signal.confidence_bps,
        );
        self.record_features(&iceberg_feature_contributions(signal, cfg));
    }

    pub fn record_liquidity_pull_signal(
        &mut self,
        signal: &LiquidityPullSignal,
        cfg: &LiquidityPullConfig,
    ) {
        self.record_common(
            SignalKind::LiquidityPull,
            signal.window_end_ns,
            signal.confidence_bps,
        );
        self.record_features(&liquidity_pull_feature_contributions(signal, cfg));
    }

    pub fn snapshot(&self) -> SignalDiagnostics {
        self.snapshot_with_top_features(Self::TOP_FEATURE_LIMIT)
    }

    pub fn snapshot_with_top_features(&self, top_feature_limit: usize) -> SignalDiagnostics {
        let absorption_score = self.absorption_score.snapshot();
        let iceberg_score = self.iceberg_score.snapshot();
        let liquidity_pull_score = self.liquidity_pull_score.snapshot();
        SignalDiagnostics {
            regime: classify_signal_regime(
                &self.counts,
                &absorption_score,
                &iceberg_score,
                &liquidity_pull_score,
            ),
            counts: self.counts.clone(),
            score: self.score.snapshot(),
            absorption_score,
            iceberg_score,
            liquidity_pull_score,
            first_signal_ns: self.first_signal_ns,
            last_signal_ns: self.last_signal_ns,
            top_features: self.top_features(top_feature_limit),
        }
    }

    fn record_common(&mut self, kind: SignalKind, window_end_ns: u64, confidence_bps: u16) {
        self.counts.total = self.counts.total.saturating_add(1);
        match kind {
            SignalKind::Absorption => {
                self.counts.absorption = self.counts.absorption.saturating_add(1);
                self.absorption_score.record(confidence_bps);
            }
            SignalKind::Iceberg => {
                self.counts.iceberg = self.counts.iceberg.saturating_add(1);
                self.iceberg_score.record(confidence_bps);
            }
            SignalKind::LiquidityPull => {
                self.counts.liquidity_pull = self.counts.liquidity_pull.saturating_add(1);
                self.liquidity_pull_score.record(confidence_bps);
            }
        }
        self.score.record(confidence_bps);
        self.first_signal_ns.get_or_insert(window_end_ns);
        self.last_signal_ns = Some(window_end_ns);
    }

    fn record_features(&mut self, contributions: &[SignalFeatureContribution]) {
        for contribution in contributions {
            let key = SignalFeatureKey {
                signal_kind: contribution.signal_kind,
                feature: contribution.feature,
            };
            let totals = self.feature_totals.entry(key).or_default();
            totals.record(contribution.score_bps, contribution.weight_pct);
        }
    }

    fn top_features(&self, limit: usize) -> Vec<SignalFeatureSummary> {
        let mut features = self
            .feature_totals
            .iter()
            .map(|(key, totals)| totals.summary(*key))
            .collect::<Vec<_>>();
        features.sort_by(|left, right| {
            right
                .total_contribution_bps
                .cmp(&left.total_contribution_bps)
                .then_with(|| right.avg_contribution_bps.cmp(&left.avg_contribution_bps))
                .then_with(|| right.observations.cmp(&left.observations))
                .then_with(|| left.signal_kind.cmp(&right.signal_kind))
                .then_with(|| left.feature.cmp(&right.feature))
        });
        features.truncate(limit);
        features
    }
}

impl ScoreAccumulator {
    fn record(&mut self, score_bps: u16) {
        self.signals = self.signals.saturating_add(1);
        self.score_sum_bps = self.score_sum_bps.saturating_add(u64::from(score_bps));
        self.max_score_bps = self.max_score_bps.max(score_bps);
    }

    fn snapshot(&self) -> SignalScoreSummary {
        SignalScoreSummary {
            signals: self.signals,
            max_score_bps: self.max_score_bps,
            avg_score_bps: avg_bps(self.score_sum_bps, self.signals),
        }
    }
}

impl FeatureScoreAccumulator {
    fn record(&mut self, score_bps: u32, weight_pct: u8) {
        let score_bps = score_bps.min(10_000);
        self.observations = self.observations.saturating_add(1);
        self.score_sum_bps = self.score_sum_bps.saturating_add(u64::from(score_bps));
        self.contribution_sum_bps = self
            .contribution_sum_bps
            .saturating_add((u64::from(score_bps) * u64::from(weight_pct)) / 100);
        self.weight_pct = weight_pct;
    }

    fn summary(&self, key: SignalFeatureKey) -> SignalFeatureSummary {
        SignalFeatureSummary {
            signal_kind: key.signal_kind,
            feature: key.feature.to_string(),
            observations: self.observations,
            weight_pct: self.weight_pct,
            avg_score_bps: avg_bps(self.score_sum_bps, self.observations),
            avg_contribution_bps: avg_bps(self.contribution_sum_bps, self.observations),
            total_contribution_bps: self.contribution_sum_bps,
        }
    }
}

fn avg_bps(sum_bps: u64, count: u64) -> u16 {
    if count == 0 {
        return 0;
    }
    (sum_bps / count).min(u64::from(u16::MAX)) as u16
}

fn avg_i128_to_i64(sum: i128, count: u64) -> i64 {
    if count == 0 {
        return 0;
    }
    (sum / i128::from(count)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn avg_u128_to_u64(sum: u128, count: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    (sum / u128::from(count)).min(u128::from(u64::MAX)) as u64
}

fn side_direction(side: Side) -> i8 {
    match side {
        Side::Bid => 1,
        Side::Ask => -1,
    }
}

fn classify_signal_regime(
    counts: &SignalCounts,
    absorption_score: &SignalScoreSummary,
    iceberg_score: &SignalScoreSummary,
    liquidity_pull_score: &SignalScoreSummary,
) -> SignalRegime {
    if counts.total == 0 {
        return SignalRegime::Balance;
    }

    let passive_count = counts.absorption.saturating_add(counts.iceberg);
    let pull_count = counts.liquidity_pull;
    if pull_count > 0 {
        let pull_share_bps = ratio_bps(pull_count, counts.total);
        if (pull_share_bps >= 5_000 && liquidity_pull_score.avg_score_bps >= 6_000)
            || (pull_count > passive_count && liquidity_pull_score.max_score_bps >= 7_000)
        {
            return SignalRegime::SpoofRisk;
        }
    }

    let passive_max_score = absorption_score
        .max_score_bps
        .max(iceberg_score.max_score_bps);
    if passive_count > 0 && passive_count >= pull_count && passive_max_score >= 5_000 {
        return SignalRegime::Absorption;
    }

    SignalRegime::InitiativeFlow
}

fn absorption_feature_contributions(
    signal: &AbsorptionSignal,
    cfg: &AbsorptionConfig,
) -> [SignalFeatureContribution; 4] {
    let pressure_score = ratio_bps(signal.executed_qty, cfg.min_executed_qty).min(10_000);
    let event_score = ratio_bps(
        u64::from(signal.execute_events),
        u64::from(cfg.min_execute_events),
    )
    .min(10_000);
    let hold_score = ratio_bps(
        signal.visible_qty_after,
        signal.visible_qty_after.saturating_add(signal.executed_qty),
    )
    .min(10_000);
    let passive_score = signal.replenishment_ratio_bps.min(10_000).max(hold_score);
    let pull_score = 10_000_u32.saturating_sub(signal.pull_ratio_bps.min(10_000));
    [
        contribution(
            SignalKind::Absorption,
            "executed_qty_pressure",
            pressure_score,
            30,
        ),
        contribution(
            SignalKind::Absorption,
            "execution_event_count",
            event_score,
            20,
        ),
        contribution(
            SignalKind::Absorption,
            "passive_replenishment_or_hold",
            passive_score,
            35,
        ),
        contribution(SignalKind::Absorption, "low_pull_ratio", pull_score, 15),
    ]
}

fn iceberg_feature_contributions(
    signal: &IcebergSignal,
    cfg: &IcebergConfig,
) -> [SignalFeatureContribution; 6] {
    let pressure_score = ratio_bps(signal.executed_qty, cfg.min_executed_qty).min(10_000);
    let execute_event_score = ratio_bps(
        u64::from(signal.execute_events),
        u64::from(cfg.min_execute_events),
    )
    .min(10_000);
    let replenish_event_score = ratio_bps(
        u64::from(signal.replenish_events),
        u64::from(cfg.min_replenish_events),
    )
    .min(10_000);
    let replenish_score = signal.replenishment_ratio_bps.min(10_000);
    let over_display_score = threshold_score_bps(
        signal.over_display_ratio_bps,
        cfg.min_over_display_ratio_bps,
    );
    let pull_score = 10_000_u32.saturating_sub(signal.pull_ratio_bps.min(10_000));
    [
        contribution(
            SignalKind::Iceberg,
            "executed_qty_pressure",
            pressure_score,
            15,
        ),
        contribution(
            SignalKind::Iceberg,
            "execution_event_count",
            execute_event_score,
            15,
        ),
        contribution(
            SignalKind::Iceberg,
            "replenish_event_count",
            replenish_event_score,
            20,
        ),
        contribution(
            SignalKind::Iceberg,
            "replenishment_ratio",
            replenish_score,
            20,
        ),
        contribution(
            SignalKind::Iceberg,
            "over_display_ratio",
            over_display_score,
            20,
        ),
        contribution(SignalKind::Iceberg, "low_pull_ratio", pull_score, 10),
    ]
}

fn liquidity_pull_feature_contributions(
    signal: &LiquidityPullSignal,
    cfg: &LiquidityPullConfig,
) -> [SignalFeatureContribution; 5] {
    let qty_score = ratio_bps(signal.pulled_qty, cfg.min_pulled_qty).min(10_000);
    let event_score = ratio_bps(
        u64::from(signal.pull_events),
        u64::from(cfg.min_pull_events),
    )
    .min(10_000);
    let pull_score = threshold_score_bps(signal.pull_ratio_bps, cfg.min_pull_ratio_bps);
    let execution_score =
        inverse_threshold_score_bps(signal.execution_ratio_bps, cfg.max_execution_ratio_bps);
    let thin_score = inverse_threshold_score_bps(
        signal.visible_after_ratio_bps,
        cfg.max_visible_after_ratio_bps,
    );
    [
        contribution(SignalKind::LiquidityPull, "pulled_qty", qty_score, 25),
        contribution(
            SignalKind::LiquidityPull,
            "pull_event_count",
            event_score,
            20,
        ),
        contribution(SignalKind::LiquidityPull, "pull_ratio", pull_score, 30),
        contribution(
            SignalKind::LiquidityPull,
            "low_execution_ratio",
            execution_score,
            15,
        ),
        contribution(SignalKind::LiquidityPull, "thin_after_pull", thin_score, 10),
    ]
}

fn contribution(
    signal_kind: SignalKind,
    feature: &'static str,
    score_bps: u32,
    weight_pct: u8,
) -> SignalFeatureContribution {
    SignalFeatureContribution {
        signal_kind,
        feature,
        score_bps,
        weight_pct,
    }
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
    pub diagnostics: SignalDiagnostics,
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
            diagnostics: SignalDiagnostics::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsorptionReplayValidation {
    pub first: AbsorptionReplayReport,
    pub second: AbsorptionReplayReport,
    pub deterministic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcebergReplayReport {
    pub frames_total: u64,
    pub control_frames: u64,
    pub parsed_events: u64,
    pub duplicate_events: u64,
    pub parse_errors: u64,
    pub signals: u64,
    pub signal_hash: u64,
    pub first_signal_ns: Option<u64>,
    pub last_signal_ns: Option<u64>,
    pub diagnostics: SignalDiagnostics,
}

impl Default for IcebergReplayReport {
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
            diagnostics: SignalDiagnostics::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IcebergReplayValidation {
    pub first: IcebergReplayReport,
    pub second: IcebergReplayReport,
    pub deterministic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidityPullReplayReport {
    pub frames_total: u64,
    pub control_frames: u64,
    pub parsed_events: u64,
    pub duplicate_events: u64,
    pub parse_errors: u64,
    pub signals: u64,
    pub signal_hash: u64,
    pub first_signal_ns: Option<u64>,
    pub last_signal_ns: Option<u64>,
    pub diagnostics: SignalDiagnostics,
}

impl Default for LiquidityPullReplayReport {
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
            diagnostics: SignalDiagnostics::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidityPullReplayValidation {
    pub first: LiquidityPullReplayReport,
    pub second: LiquidityPullReplayReport,
    pub deterministic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantReplayReport {
    pub frames_total: u64,
    pub control_frames: u64,
    pub parsed_events: u64,
    pub duplicate_events: u64,
    pub parse_errors: u64,
    pub signals: u64,
    pub signal_hash: u64,
    pub first_signal_ns: Option<u64>,
    pub last_signal_ns: Option<u64>,
    pub diagnostics: SignalDiagnostics,
    pub outcomes: SignalOutcomeSummary,
}

impl Default for ParticipantReplayReport {
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
            diagnostics: SignalDiagnostics::default(),
            outcomes: SignalOutcomeSummary::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantReplayValidation {
    pub first: ParticipantReplayReport,
    pub second: ParticipantReplayReport,
    pub deterministic: bool,
}

#[derive(Debug)]
pub struct ParticipantReplayRunner {
    absorption_cfg: AbsorptionConfig,
    iceberg_cfg: IcebergConfig,
    liquidity_pull_cfg: LiquidityPullConfig,
    absorption_detector: AbsorptionDetector,
    iceberg_detector: IcebergDetector,
    liquidity_pull_detector: LiquidityPullDetector,
    feature_engine: MicrostructureFeatureEngine,
    outcomes: SignalOutcomeTracker,
    dedupe: OboLiveDedupe,
    report: ParticipantReplayReport,
    diagnostics: SignalDiagnosticsAccumulator,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MicrostructureFeatureConfig {
    pub depth_levels: usize,
    pub z_alpha: f64,
    pub z_min_samples: u64,
    pub z_clip: f64,
}

impl Default for MicrostructureFeatureConfig {
    fn default() -> Self {
        Self {
            depth_levels: DEFAULT_FEATURE_DEPTH_LEVELS,
            z_alpha: 0.01,
            z_min_samples: 30,
            z_clip: 20.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrostructureFeatureRawValues {
    pub depth3_imb: f64,
    pub weighted_book_imb: f64,
    pub imbalance_l3: f64,
    pub ask_sz_l1: f64,
    pub touch_depth_ratio_bid: f64,
    pub cancel_touch_bid_qty_15s: f64,
    pub ofi_15s: f64,
    pub trade_cnt_imb_300s: f64,
    pub trade_touch_buy_qty_15s: f64,
    pub trade_vol_120s: f64,
    pub trade_vol_300s: f64,
    pub mom_15s: f64,
    pub mom_60s: f64,
    pub slope: f64,
    pub dow_sin: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicrostructureFeatureSnapshot {
    pub instrument_id: u64,
    pub sequence: u64,
    pub global_sequence: u64,
    pub ts_ns: u64,
    pub wall_time_ns: Option<u64>,
    pub best_bid_price_e8: Option<i64>,
    pub best_bid_qty: u64,
    pub best_ask_price_e8: Option<i64>,
    pub best_ask_qty: u64,
    pub mid_price_e8: Option<i64>,
    pub spread_e8: Option<i64>,
    pub depth3_imb_z: f64,
    pub weighted_book_imb_z: f64,
    pub imbalance_l3_z: f64,
    pub ask_sz_l1_z: f64,
    pub touch_depth_ratio_bid_z: f64,
    pub cancel_touch_bid_qty_15s_z: f64,
    pub ofi_z: f64,
    pub trade_cnt_imb_300s_z: f64,
    pub trade_touch_buy_qty_15s_z: f64,
    pub trade_vol_120s_z: f64,
    pub trade_vol_300s_z: f64,
    pub mom_15s_z: f64,
    pub mom_60s_z: f64,
    pub slope_z: f64,
    pub dow_sin: f64,
    pub raw: MicrostructureFeatureRawValues,
}

#[derive(Debug)]
pub struct MicrostructureFeatureEngine {
    cfg: MicrostructureFeatureConfig,
    instruments: HashMap<u64, InstrumentFeatureState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeatureOrderState {
    price: i64,
    qty: u64,
    side: Side,
}

#[derive(Debug, Clone, Copy, Default)]
struct FeatureEvent {
    ts_ns: u64,
    ofi_delta: f64,
    trade_qty: u64,
    trade_buy_count: u32,
    trade_sell_count: u32,
    trade_touch_buy_qty: u64,
    cancel_touch_bid_qty: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct FeatureWindowSums {
    ofi_delta: f64,
    trade_qty: u64,
    trade_buy_count: u32,
    trade_sell_count: u32,
    trade_touch_buy_qty: u64,
    cancel_touch_bid_qty: u64,
}

#[derive(Debug, Clone, Copy)]
struct DepthLevels {
    levels: [(i64, u64); MAX_FEATURE_DEPTH_LEVELS],
    len: usize,
}

#[derive(Debug)]
struct RollingFeatureWindow {
    span_ns: u64,
    events: VecDeque<FeatureEvent>,
    sums: FeatureWindowSums,
}

#[derive(Debug, Default)]
struct FeatureNormalizers {
    depth3_imb: OnlineZScore,
    weighted_book_imb: OnlineZScore,
    imbalance_l3: OnlineZScore,
    ask_sz_l1: OnlineZScore,
    touch_depth_ratio_bid: OnlineZScore,
    cancel_touch_bid_qty_15s: OnlineZScore,
    ofi_15s: OnlineZScore,
    trade_cnt_imb_300s: OnlineZScore,
    trade_touch_buy_qty_15s: OnlineZScore,
    trade_vol_120s: OnlineZScore,
    trade_vol_300s: OnlineZScore,
    mom_15s: OnlineZScore,
    mom_60s: OnlineZScore,
    slope: OnlineZScore,
}

#[derive(Debug, Clone, Copy, Default)]
struct OnlineZScore {
    samples: u64,
    mean: f64,
    variance: f64,
}

#[derive(Debug)]
struct InstrumentFeatureState {
    orders: HashMap<u64, FeatureOrderState>,
    bids: BTreeMap<i64, u64>,
    asks: BTreeMap<i64, u64>,
    last_ts_ns: u64,
    window_15s: RollingFeatureWindow,
    window_60s: RollingFeatureWindow,
    window_120s: RollingFeatureWindow,
    window_300s: RollingFeatureWindow,
    mid_history: VecDeque<(u64, i64)>,
    normalizers: FeatureNormalizers,
    last_snapshot: Option<MicrostructureFeatureSnapshot>,
}

#[derive(Debug)]
pub struct AbsorptionDetector {
    cfg: AbsorptionConfig,
    orders: HashMap<u64, OrderState>,
    levels: HashMap<LevelKey, LevelState>,
    last_ts_ns: u64,
}

#[derive(Debug)]
pub struct IcebergDetector {
    cfg: IcebergConfig,
    orders: HashMap<u64, OrderState>,
    levels: HashMap<LevelKey, IcebergLevelState>,
    last_ts_ns: u64,
}

#[derive(Debug)]
pub struct LiquidityPullDetector {
    cfg: LiquidityPullConfig,
    orders: HashMap<u64, OrderState>,
    levels: HashMap<LevelKey, LiquidityPullLevelState>,
    last_ts_ns: u64,
}

impl MicrostructureFeatureConfig {
    fn sanitized(self) -> Self {
        Self {
            depth_levels: self.depth_levels.clamp(3, MAX_FEATURE_DEPTH_LEVELS),
            z_alpha: self.z_alpha.clamp(0.000_001, 1.0),
            z_min_samples: self.z_min_samples,
            z_clip: if self.z_clip.is_finite() && self.z_clip > 0.0 {
                self.z_clip
            } else {
                20.0
            },
        }
    }
}

impl MicrostructureFeatureEngine {
    pub fn new(cfg: MicrostructureFeatureConfig) -> Self {
        Self {
            cfg: cfg.sanitized(),
            instruments: HashMap::new(),
        }
    }

    pub fn config(&self) -> MicrostructureFeatureConfig {
        self.cfg
    }

    pub fn observe_parsed(
        &mut self,
        event: &ParsedOboEvent,
        wall_time_ns: Option<u64>,
    ) -> Option<MicrostructureFeatureSnapshot> {
        let state = self
            .instruments
            .entry(event.instrument_id)
            .or_insert_with(InstrumentFeatureState::new);
        state.observe(event, wall_time_ns, self.cfg)
    }

    pub fn latest(&self, instrument_id: u64) -> Option<&MicrostructureFeatureSnapshot> {
        self.instruments
            .get(&instrument_id)
            .and_then(|state| state.last_snapshot.as_ref())
    }

    pub fn latest_all(&self) -> Vec<MicrostructureFeatureSnapshot> {
        let mut snapshots = self
            .instruments
            .values()
            .filter_map(|state| state.last_snapshot.clone())
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.instrument_id);
        snapshots
    }
}

impl Default for MicrostructureFeatureEngine {
    fn default() -> Self {
        Self::new(MicrostructureFeatureConfig::default())
    }
}

impl InstrumentFeatureState {
    fn new() -> Self {
        Self {
            orders: HashMap::new(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_ts_ns: 0,
            window_15s: RollingFeatureWindow::new(FEATURE_15S_NS),
            window_60s: RollingFeatureWindow::new(FEATURE_60S_NS),
            window_120s: RollingFeatureWindow::new(FEATURE_120S_NS),
            window_300s: RollingFeatureWindow::new(FEATURE_300S_NS),
            mid_history: VecDeque::new(),
            normalizers: FeatureNormalizers::default(),
            last_snapshot: None,
        }
    }

    fn observe(
        &mut self,
        parsed: &ParsedOboEvent,
        wall_time_ns: Option<u64>,
        cfg: MicrostructureFeatureConfig,
    ) -> Option<MicrostructureFeatureSnapshot> {
        let ts_ns = self.monotonic_ts(parsed.send_time_ns);
        let live_event = parsed.sequence != 0 || parsed.global_sequence != 0;
        let pre_best_bid = self.best_bid();
        let pre_best_ask = self.best_ask();
        let mut feature_event = FeatureEvent {
            ts_ns,
            ..FeatureEvent::default()
        };

        match parsed.event {
            OboEventV1::Add(add) => {
                let order_id = add.order_id;
                let price_e8 = add.price_e8;
                let qty = add.qty;
                let side = side_from_u8(add.side)?;
                if let Some(old) = self.orders.remove(&order_id) {
                    self.remove_depth(old.side, old.price, old.qty);
                }
                let order = FeatureOrderState {
                    price: price_e8,
                    qty,
                    side,
                };
                self.orders.insert(order_id, order);
                self.add_depth(side, price_e8, qty);
                if live_event {
                    feature_event.ofi_delta += signed_ofi_delta(side, qty as i128);
                }
            }
            OboEventV1::Modify(modify) => {
                let order_id = modify.order_id;
                let mut order = *self.orders.get(&order_id)?;
                let old_order = order;
                let new_price = if modify.flags & 1 == 1 {
                    order.price
                } else {
                    modify.new_price_e8
                };
                let new_qty = modify.new_qty;

                if old_order.price == new_price {
                    match new_qty.cmp(&old_order.qty) {
                        std::cmp::Ordering::Greater => {
                            let added = new_qty - old_order.qty;
                            self.add_depth(order.side, order.price, added);
                            if live_event {
                                feature_event.ofi_delta +=
                                    signed_ofi_delta(order.side, added as i128);
                            }
                        }
                        std::cmp::Ordering::Less => {
                            let removed = old_order.qty - new_qty;
                            self.remove_depth(order.side, order.price, removed);
                            if live_event {
                                feature_event.ofi_delta -=
                                    signed_ofi_delta(order.side, removed as i128);
                                if is_bid_touch(order.side, order.price, pre_best_bid) {
                                    feature_event.cancel_touch_bid_qty =
                                        feature_event.cancel_touch_bid_qty.saturating_add(removed);
                                }
                            }
                        }
                        std::cmp::Ordering::Equal => {}
                    }
                } else {
                    self.remove_depth(old_order.side, old_order.price, old_order.qty);
                    if live_event {
                        feature_event.ofi_delta -=
                            signed_ofi_delta(old_order.side, old_order.qty as i128);
                        if is_bid_touch(old_order.side, old_order.price, pre_best_bid) {
                            feature_event.cancel_touch_bid_qty = feature_event
                                .cancel_touch_bid_qty
                                .saturating_add(old_order.qty);
                        }
                    }
                    if new_qty > 0 {
                        self.add_depth(order.side, new_price, new_qty);
                        if live_event {
                            feature_event.ofi_delta +=
                                signed_ofi_delta(order.side, new_qty as i128);
                        }
                    }
                }

                if new_qty == 0 {
                    self.orders.remove(&order_id);
                } else {
                    order.price = new_price;
                    order.qty = new_qty;
                    self.orders.insert(order_id, order);
                }
            }
            OboEventV1::Cancel(cancel) => {
                let order_id = cancel.order_id;
                let mut order = *self.orders.get(&order_id)?;
                let removed = if cancel.qty_cxl == 0 {
                    order.qty
                } else {
                    cancel.qty_cxl.min(order.qty)
                };
                if removed == 0 {
                    self.advance_windows(ts_ns);
                    return self.snapshot(parsed, ts_ns, wall_time_ns, cfg);
                }
                self.remove_depth(order.side, order.price, removed);
                if live_event {
                    feature_event.ofi_delta -= signed_ofi_delta(order.side, removed as i128);
                    if is_bid_touch(order.side, order.price, pre_best_bid) {
                        feature_event.cancel_touch_bid_qty =
                            feature_event.cancel_touch_bid_qty.saturating_add(removed);
                    }
                }
                order.qty -= removed;
                if order.qty == 0 {
                    self.orders.remove(&order_id);
                } else {
                    self.orders.insert(order_id, order);
                }
            }
            OboEventV1::Execute(exec) => {
                let maker_order_id = exec.maker_order_id;
                let trade_qty = exec.trade_qty;
                let trade_price_e8 = exec.trade_price_e8;
                let aggressor_side = side_from_u8(exec.aggressor_side)?;
                if let Some(mut order) = self.orders.get(&maker_order_id).copied() {
                    let visible_removed = trade_qty.min(order.qty);
                    if visible_removed > 0 {
                        self.remove_depth(order.side, order.price, visible_removed);
                        if live_event {
                            feature_event.ofi_delta -=
                                signed_ofi_delta(order.side, visible_removed as i128);
                        }
                    }
                    order.qty -= visible_removed;
                    if order.qty == 0 {
                        self.orders.remove(&maker_order_id);
                    } else {
                        self.orders.insert(maker_order_id, order);
                    }
                } else if live_event {
                    feature_event.ofi_delta += trade_ofi_delta(aggressor_side, trade_qty);
                }

                if live_event && trade_qty > 0 {
                    feature_event.trade_qty = trade_qty;
                    match aggressor_side {
                        Side::Bid => {
                            feature_event.trade_buy_count = 1;
                            if pre_best_ask.is_some_and(|(price, _qty)| price == trade_price_e8) {
                                feature_event.trade_touch_buy_qty = trade_qty;
                            }
                        }
                        Side::Ask => {
                            feature_event.trade_sell_count = 1;
                        }
                    }
                }
            }
        }

        self.advance_windows(ts_ns);
        if live_event && feature_event.has_activity() {
            self.push_event(feature_event, ts_ns);
        }
        self.snapshot(parsed, ts_ns, wall_time_ns, cfg)
    }

    fn snapshot(
        &mut self,
        parsed: &ParsedOboEvent,
        ts_ns: u64,
        wall_time_ns: Option<u64>,
        cfg: MicrostructureFeatureConfig,
    ) -> Option<MicrostructureFeatureSnapshot> {
        let bids = self.top_bids(cfg.depth_levels);
        let asks = self.top_asks(cfg.depth_levels);
        let best_bid = bids.first();
        let best_ask = asks.first();
        let mid_price_e8 = best_bid
            .zip(best_ask)
            .map(|((bid_price, _), (ask_price, _))| midpoint_i64(bid_price, ask_price));
        let spread_e8 = best_bid
            .zip(best_ask)
            .and_then(|((bid_price, _), (ask_price, _))| ask_price.checked_sub(bid_price));

        if let Some(mid) = mid_price_e8 {
            self.push_mid(ts_ns, mid);
        }

        let depth3_imb = qty_imbalance(sum_qty(&bids, 3), sum_qty(&asks, 3));
        let weighted_book_imb = weighted_imbalance(&bids, &asks, cfg.depth_levels);
        let imbalance_l3 = level_imbalance(&bids, &asks, 2);
        let ask_sz_l1 = best_ask.map(|(_price, qty)| qty as f64).unwrap_or(0.0);
        let bid_depth_total = sum_qty(&bids, cfg.depth_levels);
        let touch_depth_ratio_bid = best_bid
            .map(|(_price, qty)| ratio_f64(qty, bid_depth_total))
            .unwrap_or(0.0);
        let sums_15s = self.window_15s.sums;
        let sums_120s = self.window_120s.sums;
        let sums_300s = self.window_300s.sums;
        let trade_count_total_300s =
            u64::from(sums_300s.trade_buy_count) + u64::from(sums_300s.trade_sell_count);
        let trade_cnt_imb_300s = if trade_count_total_300s == 0 {
            0.0
        } else {
            (f64::from(sums_300s.trade_buy_count) - f64::from(sums_300s.trade_sell_count))
                / trade_count_total_300s as f64
        };
        let mom_15s = mid_price_e8
            .map(|mid| self.momentum(ts_ns, FEATURE_15S_NS, mid))
            .unwrap_or(0.0);
        let mom_60s = mid_price_e8
            .map(|mid| self.momentum(ts_ns, FEATURE_60S_NS, mid))
            .unwrap_or(0.0);
        let slope = book_slope(&bids, &asks);
        let dow_sin = day_of_week_sin(wall_time_ns);

        let raw = MicrostructureFeatureRawValues {
            depth3_imb,
            weighted_book_imb,
            imbalance_l3,
            ask_sz_l1,
            touch_depth_ratio_bid,
            cancel_touch_bid_qty_15s: sums_15s.cancel_touch_bid_qty as f64,
            ofi_15s: sums_15s.ofi_delta,
            trade_cnt_imb_300s,
            trade_touch_buy_qty_15s: sums_15s.trade_touch_buy_qty as f64,
            trade_vol_120s: sums_120s.trade_qty as f64,
            trade_vol_300s: sums_300s.trade_qty as f64,
            mom_15s,
            mom_60s,
            slope,
            dow_sin,
        };

        let snapshot = MicrostructureFeatureSnapshot {
            instrument_id: parsed.instrument_id,
            sequence: parsed.sequence,
            global_sequence: parsed.global_sequence,
            ts_ns,
            wall_time_ns,
            best_bid_price_e8: best_bid.map(|(price, _qty)| price),
            best_bid_qty: best_bid.map(|(_price, qty)| qty).unwrap_or(0),
            best_ask_price_e8: best_ask.map(|(price, _qty)| price),
            best_ask_qty: best_ask.map(|(_price, qty)| qty).unwrap_or(0),
            mid_price_e8,
            spread_e8,
            depth3_imb_z: self.normalizers.depth3_imb.observe(
                raw.depth3_imb,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            weighted_book_imb_z: self.normalizers.weighted_book_imb.observe(
                raw.weighted_book_imb,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            imbalance_l3_z: self.normalizers.imbalance_l3.observe(
                raw.imbalance_l3,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            ask_sz_l1_z: self.normalizers.ask_sz_l1.observe(
                raw.ask_sz_l1,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            touch_depth_ratio_bid_z: self.normalizers.touch_depth_ratio_bid.observe(
                raw.touch_depth_ratio_bid,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            cancel_touch_bid_qty_15s_z: self.normalizers.cancel_touch_bid_qty_15s.observe(
                raw.cancel_touch_bid_qty_15s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            ofi_z: self.normalizers.ofi_15s.observe(
                raw.ofi_15s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            trade_cnt_imb_300s_z: self.normalizers.trade_cnt_imb_300s.observe(
                raw.trade_cnt_imb_300s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            trade_touch_buy_qty_15s_z: self.normalizers.trade_touch_buy_qty_15s.observe(
                raw.trade_touch_buy_qty_15s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            trade_vol_120s_z: self.normalizers.trade_vol_120s.observe(
                raw.trade_vol_120s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            trade_vol_300s_z: self.normalizers.trade_vol_300s.observe(
                raw.trade_vol_300s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            mom_15s_z: self.normalizers.mom_15s.observe(
                raw.mom_15s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            mom_60s_z: self.normalizers.mom_60s.observe(
                raw.mom_60s,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            slope_z: self.normalizers.slope.observe(
                raw.slope,
                cfg.z_alpha,
                cfg.z_min_samples,
                cfg.z_clip,
            ),
            dow_sin,
            raw,
        };
        self.last_snapshot = Some(snapshot.clone());
        Some(snapshot)
    }

    fn monotonic_ts(&mut self, ts_ns: u64) -> u64 {
        let ts_ns = ts_ns.max(self.last_ts_ns);
        self.last_ts_ns = ts_ns;
        ts_ns
    }

    fn advance_windows(&mut self, now_ns: u64) {
        self.window_15s.evict(now_ns);
        self.window_60s.evict(now_ns);
        self.window_120s.evict(now_ns);
        self.window_300s.evict(now_ns);
        self.evict_mid_history(now_ns);
    }

    fn push_event(&mut self, event: FeatureEvent, now_ns: u64) {
        self.window_15s.push(event, now_ns);
        self.window_60s.push(event, now_ns);
        self.window_120s.push(event, now_ns);
        self.window_300s.push(event, now_ns);
    }

    fn add_depth(&mut self, side: Side, price: i64, qty: u64) {
        if qty == 0 {
            return;
        }
        let map = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let total = map.entry(price).or_insert(0);
        *total = total.saturating_add(qty);
    }

    fn remove_depth(&mut self, side: Side, price: i64, qty: u64) {
        if qty == 0 {
            return;
        }
        let map = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let remove_level = if let Some(total) = map.get_mut(&price) {
            *total = total.saturating_sub(qty);
            *total == 0
        } else {
            false
        };
        if remove_level {
            map.remove(&price);
        }
    }

    fn best_bid(&self) -> Option<(i64, u64)> {
        self.bids
            .iter()
            .next_back()
            .map(|(price, qty)| (*price, *qty))
    }

    fn best_ask(&self) -> Option<(i64, u64)> {
        self.asks.iter().next().map(|(price, qty)| (*price, *qty))
    }

    fn top_bids(&self, depth: usize) -> DepthLevels {
        let mut levels = DepthLevels::new();
        for (price, qty) in self.bids.iter().rev().take(depth) {
            levels.push(*price, *qty);
        }
        levels
    }

    fn top_asks(&self, depth: usize) -> DepthLevels {
        let mut levels = DepthLevels::new();
        for (price, qty) in self.asks.iter().take(depth) {
            levels.push(*price, *qty);
        }
        levels
    }

    fn push_mid(&mut self, ts_ns: u64, mid_price_e8: i64) {
        if self
            .mid_history
            .back()
            .is_some_and(|(last_ts, last_mid)| *last_ts == ts_ns && *last_mid == mid_price_e8)
        {
            return;
        }
        self.mid_history.push_back((ts_ns, mid_price_e8));
        self.evict_mid_history(ts_ns);
    }

    fn evict_mid_history(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(FEATURE_60S_NS);
        while self.mid_history.len() > 1 {
            let second_ts = self.mid_history.get(1).map(|(ts, _mid)| *ts);
            if second_ts.is_some_and(|ts| ts <= cutoff) {
                self.mid_history.pop_front();
            } else {
                break;
            }
        }
    }

    fn momentum(&self, now_ns: u64, window_ns: u64, current_mid: i64) -> f64 {
        let cutoff = now_ns.saturating_sub(window_ns);
        let mut anchor = None;
        for (ts, mid) in self.mid_history.iter().rev() {
            if *ts <= cutoff {
                anchor = Some(*mid);
                break;
            }
        }
        anchor
            .map(|past_mid| current_mid.saturating_sub(past_mid) as f64)
            .unwrap_or(0.0)
    }
}

impl FeatureEvent {
    fn has_activity(self) -> bool {
        self.ofi_delta != 0.0
            || self.trade_qty != 0
            || self.trade_buy_count != 0
            || self.trade_sell_count != 0
            || self.trade_touch_buy_qty != 0
            || self.cancel_touch_bid_qty != 0
    }
}

impl DepthLevels {
    fn new() -> Self {
        Self {
            levels: [(0, 0); MAX_FEATURE_DEPTH_LEVELS],
            len: 0,
        }
    }

    fn push(&mut self, price: i64, qty: u64) {
        if self.len >= MAX_FEATURE_DEPTH_LEVELS {
            return;
        }
        self.levels[self.len] = (price, qty);
        self.len += 1;
    }

    fn first(&self) -> Option<(i64, u64)> {
        self.get(0)
    }

    fn get(&self, idx: usize) -> Option<(i64, u64)> {
        (idx < self.len).then_some(self.levels[idx])
    }

    fn iter(&self) -> impl Iterator<Item = (i64, u64)> + '_ {
        self.levels[..self.len].iter().copied()
    }
}

impl RollingFeatureWindow {
    fn new(span_ns: u64) -> Self {
        Self {
            span_ns,
            events: VecDeque::new(),
            sums: FeatureWindowSums::default(),
        }
    }

    fn push(&mut self, event: FeatureEvent, now_ns: u64) {
        self.add_to_sums(event);
        self.events.push_back(event);
        self.evict(now_ns);
    }

    fn evict(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(self.span_ns);
        while self
            .events
            .front()
            .is_some_and(|event| event.ts_ns < cutoff)
        {
            if let Some(event) = self.events.pop_front() {
                self.remove_from_sums(event);
            }
        }
    }

    fn add_to_sums(&mut self, event: FeatureEvent) {
        self.sums.ofi_delta += event.ofi_delta;
        self.sums.trade_qty = self.sums.trade_qty.saturating_add(event.trade_qty);
        self.sums.trade_buy_count = self
            .sums
            .trade_buy_count
            .saturating_add(event.trade_buy_count);
        self.sums.trade_sell_count = self
            .sums
            .trade_sell_count
            .saturating_add(event.trade_sell_count);
        self.sums.trade_touch_buy_qty = self
            .sums
            .trade_touch_buy_qty
            .saturating_add(event.trade_touch_buy_qty);
        self.sums.cancel_touch_bid_qty = self
            .sums
            .cancel_touch_bid_qty
            .saturating_add(event.cancel_touch_bid_qty);
    }

    fn remove_from_sums(&mut self, event: FeatureEvent) {
        self.sums.ofi_delta -= event.ofi_delta;
        self.sums.trade_qty = self.sums.trade_qty.saturating_sub(event.trade_qty);
        self.sums.trade_buy_count = self
            .sums
            .trade_buy_count
            .saturating_sub(event.trade_buy_count);
        self.sums.trade_sell_count = self
            .sums
            .trade_sell_count
            .saturating_sub(event.trade_sell_count);
        self.sums.trade_touch_buy_qty = self
            .sums
            .trade_touch_buy_qty
            .saturating_sub(event.trade_touch_buy_qty);
        self.sums.cancel_touch_bid_qty = self
            .sums
            .cancel_touch_bid_qty
            .saturating_sub(event.cancel_touch_bid_qty);
    }
}

impl OnlineZScore {
    fn observe(&mut self, value: f64, alpha: f64, min_samples: u64, clip: f64) -> f64 {
        let z = if self.samples >= min_samples && self.variance > f64::EPSILON {
            ((value - self.mean) / self.variance.sqrt()).clamp(-clip, clip)
        } else {
            0.0
        };

        if self.samples == 0 {
            self.mean = value;
            self.variance = 0.0;
        } else {
            let delta = value - self.mean;
            self.mean += alpha * delta;
            self.variance = (1.0 - alpha) * (self.variance + alpha * delta * delta);
        }
        self.samples = self.samples.saturating_add(1);
        if z.is_finite() {
            z
        } else {
            0.0
        }
    }
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

impl IcebergDetector {
    pub fn new(cfg: IcebergConfig) -> Self {
        Self {
            cfg,
            orders: HashMap::new(),
            levels: HashMap::new(),
            last_ts_ns: 0,
        }
    }

    pub fn config(&self) -> IcebergConfig {
        self.cfg
    }

    pub fn observe_obo(
        &mut self,
        ts_ns: u64,
        instrument_id: u64,
        event: OboEventV1,
    ) -> Option<IcebergSignal> {
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
    ) -> Result<Option<IcebergSignal>, OboFrameError> {
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
    ) -> Option<IcebergSignal> {
        let level = self.levels.entry(key).or_default();
        level.evict(ts_ns, self.cfg.window_ns);
        let visible_before = level.visible_qty;

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
        level.observations.push_back(IcebergObservation {
            ts_ns,
            qty,
            kind,
            visible_before,
            visible_after: level.visible_qty,
        });
        level.maybe_signal(key, ts_ns, &self.cfg)
    }
}

impl LiquidityPullDetector {
    pub fn new(cfg: LiquidityPullConfig) -> Self {
        Self {
            cfg,
            orders: HashMap::new(),
            levels: HashMap::new(),
            last_ts_ns: 0,
        }
    }

    pub fn config(&self) -> LiquidityPullConfig {
        self.cfg
    }

    pub fn observe_obo(
        &mut self,
        ts_ns: u64,
        instrument_id: u64,
        event: OboEventV1,
    ) -> Option<LiquidityPullSignal> {
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
                let pulled_side = opposite_side(aggressor_side);
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
                                side: pulled_side,
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
    ) -> Result<Option<LiquidityPullSignal>, OboFrameError> {
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
    ) -> Option<LiquidityPullSignal> {
        let level = self.levels.entry(key).or_default();
        level.evict(ts_ns, self.cfg.window_ns);
        let visible_before = level.visible_qty;

        if visible_delta >= 0 {
            level.visible_qty = level.visible_qty.saturating_add(visible_delta as u64);
        } else {
            level.visible_qty = level.visible_qty.saturating_sub((-visible_delta) as u64);
        }

        let (kind, qty) = observation?;
        if qty == 0 {
            return None;
        }
        level.observations.push_back(IcebergObservation {
            ts_ns,
            qty,
            kind,
            visible_before,
            visible_after: level.visible_qty,
        });
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
    let mut diagnostics = SignalDiagnosticsAccumulator::default();
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
                    diagnostics.record_absorption_signal(&signal, &cfg);
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
    report.diagnostics = diagnostics.snapshot();
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

pub fn replay_iceberg_frames<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: IcebergConfig,
) -> IcebergReplayReport {
    let mut detector = IcebergDetector::new(cfg);
    let mut dedupe = OboLiveDedupe::new();
    let mut report = IcebergReplayReport::default();
    let mut diagnostics = SignalDiagnosticsAccumulator::default();
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
                    diagnostics.record_iceberg_signal(&signal, &cfg);
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
    report.diagnostics = diagnostics.snapshot();
    report
}

pub fn validate_iceberg_replay<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: IcebergConfig,
) -> IcebergReplayValidation {
    let first = replay_iceberg_frames(frames, cfg);
    let second = replay_iceberg_frames(frames, cfg);
    let deterministic = first == second;
    IcebergReplayValidation {
        first,
        second,
        deterministic,
    }
}

pub fn replay_liquidity_pull_frames<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: LiquidityPullConfig,
) -> LiquidityPullReplayReport {
    let mut detector = LiquidityPullDetector::new(cfg);
    let mut dedupe = OboLiveDedupe::new();
    let mut report = LiquidityPullReplayReport::default();
    let mut diagnostics = SignalDiagnosticsAccumulator::default();
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
                    diagnostics.record_liquidity_pull_signal(&signal, &cfg);
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
    report.diagnostics = diagnostics.snapshot();
    report
}

pub fn validate_liquidity_pull_replay<B: AsRef<[u8]>>(
    frames: &[B],
    cfg: LiquidityPullConfig,
) -> LiquidityPullReplayValidation {
    let first = replay_liquidity_pull_frames(frames, cfg);
    let second = replay_liquidity_pull_frames(frames, cfg);
    let deterministic = first == second;
    LiquidityPullReplayValidation {
        first,
        second,
        deterministic,
    }
}

pub fn replay_participant_frames<B: AsRef<[u8]>>(
    frames: &[B],
    absorption_cfg: AbsorptionConfig,
    iceberg_cfg: IcebergConfig,
    liquidity_pull_cfg: LiquidityPullConfig,
) -> ParticipantReplayReport {
    let mut runner = ParticipantReplayRunner::new(absorption_cfg, iceberg_cfg, liquidity_pull_cfg);
    for frame in frames {
        runner.observe_frame(frame.as_ref());
    }
    runner.finish()
}

impl ParticipantReplayRunner {
    pub fn new(
        absorption_cfg: AbsorptionConfig,
        iceberg_cfg: IcebergConfig,
        liquidity_pull_cfg: LiquidityPullConfig,
    ) -> Self {
        Self {
            absorption_cfg,
            iceberg_cfg,
            liquidity_pull_cfg,
            absorption_detector: AbsorptionDetector::new(absorption_cfg),
            iceberg_detector: IcebergDetector::new(iceberg_cfg),
            liquidity_pull_detector: LiquidityPullDetector::new(liquidity_pull_cfg),
            feature_engine: MicrostructureFeatureEngine::default(),
            outcomes: SignalOutcomeTracker::default(),
            dedupe: OboLiveDedupe::new(),
            report: ParticipantReplayReport::default(),
            diagnostics: SignalDiagnosticsAccumulator::default(),
        }
    }

    pub fn observe_frame(&mut self, frame: &[u8]) {
        self.report.frames_total = self.report.frames_total.saturating_add(1);
        match parse_obo_frame(frame) {
            Ok(Some(parsed)) => {
                if !self.dedupe.accept(&parsed) {
                    self.report.duplicate_events = self.report.duplicate_events.saturating_add(1);
                    return;
                }
                self.report.parsed_events = self.report.parsed_events.saturating_add(1);
                let feature_snapshot = self.feature_engine.observe_parsed(&parsed, None);
                if let Some(snapshot) = feature_snapshot.as_ref() {
                    self.outcomes.observe_snapshot(snapshot);
                }
                if let Some(signal) = self.absorption_detector.observe_obo(
                    parsed.send_time_ns,
                    parsed.instrument_id,
                    parsed.event,
                ) {
                    let signal = ParticipantSignal::Absorption(signal);
                    self.report.record_signal(&signal);
                    self.diagnostics.record_signal(
                        &signal,
                        &self.absorption_cfg,
                        &self.iceberg_cfg,
                        &self.liquidity_pull_cfg,
                    );
                    if let Some(snapshot) = feature_snapshot.as_ref() {
                        self.outcomes.track_signal(&signal, snapshot);
                    }
                }
                if let Some(signal) = self.iceberg_detector.observe_obo(
                    parsed.send_time_ns,
                    parsed.instrument_id,
                    parsed.event,
                ) {
                    let signal = ParticipantSignal::Iceberg(signal);
                    self.report.record_signal(&signal);
                    self.diagnostics.record_signal(
                        &signal,
                        &self.absorption_cfg,
                        &self.iceberg_cfg,
                        &self.liquidity_pull_cfg,
                    );
                    if let Some(snapshot) = feature_snapshot.as_ref() {
                        self.outcomes.track_signal(&signal, snapshot);
                    }
                }
                if let Some(signal) = self.liquidity_pull_detector.observe_obo(
                    parsed.send_time_ns,
                    parsed.instrument_id,
                    parsed.event,
                ) {
                    let signal = ParticipantSignal::LiquidityPull(signal);
                    self.report.record_signal(&signal);
                    self.diagnostics.record_signal(
                        &signal,
                        &self.absorption_cfg,
                        &self.iceberg_cfg,
                        &self.liquidity_pull_cfg,
                    );
                    if let Some(snapshot) = feature_snapshot.as_ref() {
                        self.outcomes.track_signal(&signal, snapshot);
                    }
                }
            }
            Ok(None) => {
                self.report.control_frames = self.report.control_frames.saturating_add(1);
            }
            Err(_err) => {
                self.report.parse_errors = self.report.parse_errors.saturating_add(1);
            }
        }
    }

    pub fn finish(mut self) -> ParticipantReplayReport {
        self.report.diagnostics = self.diagnostics.snapshot();
        self.report.outcomes = self.outcomes.snapshot();
        self.report
    }
}

pub fn validate_participant_replay<B: AsRef<[u8]>>(
    frames: &[B],
    absorption_cfg: AbsorptionConfig,
    iceberg_cfg: IcebergConfig,
    liquidity_pull_cfg: LiquidityPullConfig,
) -> ParticipantReplayValidation {
    let first = replay_participant_frames(frames, absorption_cfg, iceberg_cfg, liquidity_pull_cfg);
    let second = replay_participant_frames(frames, absorption_cfg, iceberg_cfg, liquidity_pull_cfg);
    let deterministic = first == second;
    ParticipantReplayValidation {
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

impl IcebergReplayReport {
    fn record_signal(&mut self, signal: &IcebergSignal) {
        self.signals = self.signals.saturating_add(1);
        self.first_signal_ns.get_or_insert(signal.window_end_ns);
        self.last_signal_ns = Some(signal.window_end_ns);
        hash_iceberg_signal(&mut self.signal_hash, signal);
    }
}

impl LiquidityPullReplayReport {
    fn record_signal(&mut self, signal: &LiquidityPullSignal) {
        self.signals = self.signals.saturating_add(1);
        self.first_signal_ns.get_or_insert(signal.window_end_ns);
        self.last_signal_ns = Some(signal.window_end_ns);
        hash_liquidity_pull_signal(&mut self.signal_hash, signal);
    }
}

impl ParticipantReplayReport {
    fn record_signal(&mut self, signal: &ParticipantSignal) {
        self.signals = self.signals.saturating_add(1);
        self.first_signal_ns.get_or_insert(signal.window_end_ns());
        self.last_signal_ns = Some(signal.window_end_ns());
        hash_participant_signal(&mut self.signal_hash, signal);
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

impl IcebergLevelState {
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
        cfg: &IcebergConfig,
    ) -> Option<IcebergSignal> {
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
        let mut max_visible_qty = self.visible_qty;
        let mut execute_events = 0_u32;
        let mut replenish_events = 0_u32;
        let mut pull_events = 0_u32;

        for obs in &self.observations {
            window_start_ns = window_start_ns.min(obs.ts_ns);
            max_visible_qty = max_visible_qty
                .max(obs.visible_before)
                .max(obs.visible_after);
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

        if executed_qty < cfg.min_executed_qty
            || execute_events < cfg.min_execute_events
            || replenish_events < cfg.min_replenish_events
            || replenished_qty < cfg.min_replenished_qty
            || max_visible_qty == 0
        {
            return None;
        }

        let replenishment_ratio_bps = ratio_bps(replenished_qty, executed_qty);
        let over_display_ratio_bps = ratio_bps(executed_qty, max_visible_qty);
        let pull_ratio_bps = ratio_bps(pulled_qty, executed_qty);
        if replenishment_ratio_bps < cfg.min_replenishment_ratio_bps
            || over_display_ratio_bps < cfg.min_over_display_ratio_bps
            || pull_ratio_bps > cfg.max_pull_ratio_bps
        {
            return None;
        }

        let confidence_bps = iceberg_confidence_bps(
            executed_qty,
            execute_events,
            replenish_events,
            replenishment_ratio_bps,
            over_display_ratio_bps,
            pull_ratio_bps,
            cfg,
        );
        self.last_signal_ns = Some(now_ns);
        Some(IcebergSignal {
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
            max_visible_qty,
            execute_events,
            replenish_events,
            pull_events,
            replenishment_ratio_bps,
            over_display_ratio_bps,
            pull_ratio_bps,
            confidence_bps,
        })
    }
}

impl LiquidityPullLevelState {
    fn evict(&mut self, now_ns: u64, window_ns: u64) {
        let cutoff = now_ns.saturating_sub(window_ns);
        while let Some(front) = self.observations.front() {
            if front.ts_ns >= cutoff {
                break;
            }
            self.observations.pop_front();
        }
    }

    fn maybe_signal(
        &mut self,
        key: LevelKey,
        now_ns: u64,
        cfg: &LiquidityPullConfig,
    ) -> Option<LiquidityPullSignal> {
        if self
            .last_signal_ns
            .is_some_and(|last| now_ns.saturating_sub(last) < cfg.cooldown_ns)
        {
            return None;
        }

        let mut window_start_ns = now_ns;
        let mut pulled_qty = 0_u64;
        let mut executed_qty = 0_u64;
        let mut replenished_qty = 0_u64;
        let mut max_visible_qty = self.visible_qty;
        let mut pull_events = 0_u32;
        let mut execute_events = 0_u32;
        let mut replenish_events = 0_u32;

        for obs in &self.observations {
            window_start_ns = window_start_ns.min(obs.ts_ns);
            max_visible_qty = max_visible_qty
                .max(obs.visible_before)
                .max(obs.visible_after);
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

        if pulled_qty < cfg.min_pulled_qty
            || pull_events < cfg.min_pull_events
            || max_visible_qty < cfg.min_visible_qty
            || max_visible_qty == 0
        {
            return None;
        }

        let pull_ratio_bps = ratio_bps(pulled_qty, max_visible_qty);
        let execution_ratio_bps = ratio_bps(executed_qty, pulled_qty);
        let visible_after_ratio_bps = ratio_bps(self.visible_qty, max_visible_qty);
        if pull_ratio_bps < cfg.min_pull_ratio_bps
            || execution_ratio_bps > cfg.max_execution_ratio_bps
            || visible_after_ratio_bps > cfg.max_visible_after_ratio_bps
        {
            return None;
        }

        let confidence_bps = liquidity_pull_confidence_bps(
            pulled_qty,
            pull_events,
            pull_ratio_bps,
            execution_ratio_bps,
            visible_after_ratio_bps,
            cfg,
        );
        self.last_signal_ns = Some(now_ns);
        Some(LiquidityPullSignal {
            instrument_id: key.instrument_id,
            price: key.price,
            pulled_side: key.side,
            opposing_side: opposite_side(key.side),
            window_start_ns,
            window_end_ns: now_ns,
            pulled_qty,
            executed_qty,
            replenished_qty,
            visible_qty_after: self.visible_qty,
            max_visible_qty,
            pull_events,
            execute_events,
            replenish_events,
            pull_ratio_bps,
            execution_ratio_bps,
            visible_after_ratio_bps,
            confidence_bps,
        })
    }
}

fn signed_ofi_delta(side: Side, qty_delta: i128) -> f64 {
    match side {
        Side::Bid => qty_delta as f64,
        Side::Ask => -(qty_delta as f64),
    }
}

fn trade_ofi_delta(aggressor_side: Side, qty: u64) -> f64 {
    match aggressor_side {
        Side::Bid => qty as f64,
        Side::Ask => -(qty as f64),
    }
}

fn is_bid_touch(side: Side, price: i64, best_bid: Option<(i64, u64)>) -> bool {
    side == Side::Bid && best_bid.is_some_and(|(best_price, _qty)| best_price == price)
}

fn sum_qty(levels: &DepthLevels, depth: usize) -> u64 {
    levels
        .iter()
        .take(depth)
        .fold(0_u64, |acc, (_price, qty)| acc.saturating_add(qty))
}

fn qty_imbalance(bid_qty: u64, ask_qty: u64) -> f64 {
    let total = bid_qty.saturating_add(ask_qty);
    if total == 0 {
        0.0
    } else {
        (bid_qty as f64 - ask_qty as f64) / total as f64
    }
}

fn weighted_imbalance(bids: &DepthLevels, asks: &DepthLevels, depth: usize) -> f64 {
    let mut bid_weighted = 0.0;
    let mut ask_weighted = 0.0;
    for level in 0..depth {
        let weight = 1.0 / (level as f64 + 1.0);
        if let Some((_price, qty)) = bids.get(level) {
            bid_weighted += qty as f64 * weight;
        }
        if let Some((_price, qty)) = asks.get(level) {
            ask_weighted += qty as f64 * weight;
        }
    }
    let total = bid_weighted + ask_weighted;
    if total == 0.0 {
        0.0
    } else {
        (bid_weighted - ask_weighted) / total
    }
}

fn level_imbalance(bids: &DepthLevels, asks: &DepthLevels, level_idx: usize) -> f64 {
    let bid_qty = bids.get(level_idx).map(|(_price, qty)| qty).unwrap_or(0);
    let ask_qty = asks.get(level_idx).map(|(_price, qty)| qty).unwrap_or(0);
    qty_imbalance(bid_qty, ask_qty)
}

fn ratio_f64(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn midpoint_i64(bid_price: i64, ask_price: i64) -> i64 {
    let midpoint = (bid_price as i128 + ask_price as i128) / 2;
    midpoint.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn book_slope(bids: &DepthLevels, asks: &DepthLevels) -> f64 {
    let Some((best_bid, _)) = bids.first() else {
        return 0.0;
    };
    let Some((best_ask, _)) = asks.first() else {
        return 0.0;
    };
    let tick = estimate_tick(bids, asks) as f64;
    let mid = (best_bid as f64 + best_ask as f64) / 2.0;
    let mut weighted_density = 0.0;
    let mut total_qty = 0.0;
    for (price, qty) in bids.iter().chain(asks.iter()) {
        let distance_ticks = ((price as f64 - mid).abs() / tick).max(0.5);
        weighted_density += qty as f64 / distance_ticks;
        total_qty += qty as f64;
    }
    if total_qty == 0.0 {
        0.0
    } else {
        weighted_density / total_qty
    }
}

fn estimate_tick(bids: &DepthLevels, asks: &DepthLevels) -> i64 {
    let mut tick = i64::MAX;
    update_tick_from_levels(&mut tick, bids);
    update_tick_from_levels(&mut tick, asks);
    if tick == i64::MAX {
        1
    } else {
        tick.max(1)
    }
}

fn update_tick_from_levels(tick: &mut i64, levels: &DepthLevels) {
    let mut previous: Option<i64> = None;
    for (price, _qty) in levels.iter() {
        if let Some(previous_price) = previous {
            let diff = previous_price.saturating_sub(price).abs();
            if diff > 0 {
                *tick = (*tick).min(diff);
            }
        }
        previous = Some(price);
    }
}

fn day_of_week_sin(wall_time_ns: Option<u64>) -> f64 {
    let Some(wall_time_ns) = wall_time_ns else {
        return 0.0;
    };
    let seconds = (wall_time_ns / NS_PER_SECOND) as i64;
    let Some(dt) = chrono::DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return 0.0;
    };
    let day = f64::from(dt.weekday().num_days_from_monday());
    ((day / 7.0) * std::f64::consts::TAU).sin()
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

fn iceberg_confidence_bps(
    executed_qty: u64,
    execute_events: u32,
    replenish_events: u32,
    replenishment_ratio_bps: u32,
    over_display_ratio_bps: u32,
    pull_ratio_bps: u32,
    cfg: &IcebergConfig,
) -> u16 {
    let pressure_score = ratio_bps(executed_qty, cfg.min_executed_qty).min(10_000);
    let execute_event_score =
        ratio_bps(u64::from(execute_events), u64::from(cfg.min_execute_events)).min(10_000);
    let replenish_event_score = ratio_bps(
        u64::from(replenish_events),
        u64::from(cfg.min_replenish_events),
    )
    .min(10_000);
    let replenish_score = replenishment_ratio_bps.min(10_000);
    let over_display_score = ratio_bps(
        u64::from(over_display_ratio_bps),
        u64::from(cfg.min_over_display_ratio_bps),
    )
    .min(10_000);
    let pull_score = 10_000_u32.saturating_sub(pull_ratio_bps.min(10_000));
    let weighted = pressure_score * 15
        + execute_event_score * 15
        + replenish_event_score * 20
        + replenish_score * 20
        + over_display_score * 20
        + pull_score * 10;
    (weighted / 100).min(10_000) as u16
}

fn liquidity_pull_confidence_bps(
    pulled_qty: u64,
    pull_events: u32,
    pull_ratio_bps: u32,
    execution_ratio_bps: u32,
    visible_after_ratio_bps: u32,
    cfg: &LiquidityPullConfig,
) -> u16 {
    let qty_score = ratio_bps(pulled_qty, cfg.min_pulled_qty).min(10_000);
    let event_score = ratio_bps(u64::from(pull_events), u64::from(cfg.min_pull_events)).min(10_000);
    let pull_score = threshold_score_bps(pull_ratio_bps, cfg.min_pull_ratio_bps);
    let execution_score =
        inverse_threshold_score_bps(execution_ratio_bps, cfg.max_execution_ratio_bps);
    let thin_score =
        inverse_threshold_score_bps(visible_after_ratio_bps, cfg.max_visible_after_ratio_bps);
    let weighted = qty_score * 25
        + event_score * 20
        + pull_score * 30
        + execution_score * 15
        + thin_score * 10;
    (weighted / 100).min(10_000) as u16
}

fn threshold_score_bps(value_bps: u32, threshold_bps: u32) -> u32 {
    if threshold_bps == 0 {
        return 10_000;
    }
    ratio_bps(u64::from(value_bps), u64::from(threshold_bps)).min(10_000)
}

fn inverse_threshold_score_bps(value_bps: u32, threshold_bps: u32) -> u32 {
    if threshold_bps == 0 {
        return if value_bps == 0 { 10_000 } else { 0 };
    }
    10_000_u32.saturating_sub(ratio_bps(u64::from(value_bps), u64::from(threshold_bps)).min(10_000))
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

fn hash_iceberg_signal(hash: &mut u64, signal: &IcebergSignal) {
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
    hash_u64(hash, signal.max_visible_qty);
    hash_u32(hash, signal.execute_events);
    hash_u32(hash, signal.replenish_events);
    hash_u32(hash, signal.pull_events);
    hash_u32(hash, signal.replenishment_ratio_bps);
    hash_u32(hash, signal.over_display_ratio_bps);
    hash_u32(hash, signal.pull_ratio_bps);
    hash_u16(hash, signal.confidence_bps);
}

fn hash_liquidity_pull_signal(hash: &mut u64, signal: &LiquidityPullSignal) {
    hash_u64(hash, signal.instrument_id);
    hash_i64(hash, signal.price);
    hash_u8(hash, side_to_u8(signal.pulled_side));
    hash_u8(hash, side_to_u8(signal.opposing_side));
    hash_u64(hash, signal.window_start_ns);
    hash_u64(hash, signal.window_end_ns);
    hash_u64(hash, signal.pulled_qty);
    hash_u64(hash, signal.executed_qty);
    hash_u64(hash, signal.replenished_qty);
    hash_u64(hash, signal.visible_qty_after);
    hash_u64(hash, signal.max_visible_qty);
    hash_u32(hash, signal.pull_events);
    hash_u32(hash, signal.execute_events);
    hash_u32(hash, signal.replenish_events);
    hash_u32(hash, signal.pull_ratio_bps);
    hash_u32(hash, signal.execution_ratio_bps);
    hash_u32(hash, signal.visible_after_ratio_bps);
    hash_u16(hash, signal.confidence_bps);
}

fn hash_participant_signal(hash: &mut u64, signal: &ParticipantSignal) {
    hash_u8(hash, signal_kind_to_u8(signal.kind()));
    match signal {
        ParticipantSignal::Absorption(signal) => hash_signal(hash, signal),
        ParticipantSignal::Iceberg(signal) => hash_iceberg_signal(hash, signal),
        ParticipantSignal::LiquidityPull(signal) => hash_liquidity_pull_signal(hash, signal),
    }
}

fn signal_kind_to_u8(kind: SignalKind) -> u8 {
    match kind {
        SignalKind::Absorption => 1,
        SignalKind::Iceberg => 2,
        SignalKind::LiquidityPull => 3,
    }
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

    fn iceberg_cfg() -> IcebergConfig {
        IcebergConfig {
            window_ns: 1_000,
            min_executed_qty: 100,
            min_execute_events: 2,
            min_replenish_events: 2,
            min_replenished_qty: 100,
            min_replenishment_ratio_bps: 5_000,
            min_over_display_ratio_bps: 12_000,
            max_pull_ratio_bps: 2_500,
            cooldown_ns: 500,
        }
    }

    fn liquidity_pull_cfg() -> LiquidityPullConfig {
        LiquidityPullConfig {
            window_ns: 1_000,
            min_pulled_qty: 250,
            min_pull_events: 2,
            min_visible_qty: 400,
            min_pull_ratio_bps: 5_000,
            max_execution_ratio_bps: 2_500,
            max_visible_after_ratio_bps: 5_000,
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
        assert_eq!(validation.first.diagnostics.counts.total, 1);
        assert_eq!(validation.first.diagnostics.counts.absorption, 1);
        assert_eq!(
            validation.first.diagnostics.regime,
            SignalRegime::Absorption
        );
        assert_eq!(validation.first.diagnostics.score.signals, 1);
        assert!(!validation.first.diagnostics.top_features.is_empty());
        assert_eq!(validation.first, validation.second);
    }

    #[test]
    fn detects_iceberg_candidate_from_repeated_replenishment() {
        let mut detector = IcebergDetector::new(iceberg_cfg());
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 60, Side::Ask));
        detector.observe_obo(30, 7, add(2, 100, 60, Side::Bid));
        detector.observe_obo(40, 7, execute(2, 100, 60, Side::Ask));
        let signal = detector
            .observe_obo(50, 7, add(3, 100, 60, Side::Bid))
            .expect("iceberg candidate signal");
        assert_eq!(signal.instrument_id, 7);
        assert_eq!(signal.price, 100);
        assert_eq!(signal.passive_side, Side::Bid);
        assert_eq!(signal.aggressor_side, Side::Ask);
        assert_eq!(signal.executed_qty, 120);
        assert_eq!(signal.replenished_qty, 120);
        assert_eq!(signal.max_visible_qty, 100);
        assert_eq!(signal.visible_qty_after, 100);
        assert_eq!(signal.execute_events, 2);
        assert_eq!(signal.replenish_events, 2);
        assert_eq!(signal.over_display_ratio_bps, 12_000);
        assert!(signal.confidence_bps >= 8_000);
    }

    #[test]
    fn iceberg_rejects_single_refill_as_insufficient_cycles() {
        let mut detector = IcebergDetector::new(iceberg_cfg());
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, execute(1, 100, 70, Side::Ask));
        detector.observe_obo(30, 7, add(2, 100, 70, Side::Bid));
        let signal = detector.observe_obo(40, 7, execute(2, 100, 50, Side::Ask));
        assert!(signal.is_none());
    }

    #[test]
    fn iceberg_ignores_replenishment_before_execution_pressure() {
        let mut config = iceberg_cfg();
        config.min_execute_events = 1;
        config.min_replenish_events = 1;
        config.min_over_display_ratio_bps = 5_000;
        let mut detector = IcebergDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 100, Side::Bid));
        detector.observe_obo(20, 7, add(2, 100, 100, Side::Bid));
        let signal = detector.observe_obo(30, 7, execute(1, 100, 100, Side::Ask));
        assert!(signal.is_none());
    }

    #[test]
    fn iceberg_raw_frame_parser_feeds_detector() {
        let mut config = iceberg_cfg();
        config.min_execute_events = 1;
        config.min_replenish_events = 1;
        config.min_executed_qty = 80;
        config.min_replenished_qty = 50;
        config.min_over_display_ratio_bps = 8_000;
        let mut detector = IcebergDetector::new(config);
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
            .expect("raw frame iceberg signal");
        assert_eq!(signal.executed_qty, 80);
        assert_eq!(signal.replenished_qty, 60);
        assert_eq!(signal.max_visible_qty, 100);
    }

    #[test]
    fn iceberg_replay_validation_is_deterministic_and_dedupes_live_frames() {
        let mut config = iceberg_cfg();
        config.min_execute_events = 1;
        config.min_replenish_events = 1;
        config.min_executed_qty = 80;
        config.min_replenished_qty = 50;
        config.min_over_display_ratio_bps = 8_000;
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

        let validation = validate_iceberg_replay(&frames, config);
        assert!(validation.deterministic);
        assert_eq!(validation.first.frames_total, 4);
        assert_eq!(validation.first.parsed_events, 3);
        assert_eq!(validation.first.duplicate_events, 1);
        assert_eq!(validation.first.signals, 1);
        assert_ne!(validation.first.signal_hash, REPLAY_HASH_OFFSET);
        assert_eq!(validation.first, validation.second);
    }

    #[test]
    fn detects_liquidity_pull_from_cancels_and_qty_reductions() {
        let mut detector = LiquidityPullDetector::new(liquidity_pull_cfg());
        detector.observe_obo(10, 7, add(1, 100, 250, Side::Ask));
        detector.observe_obo(20, 7, add(2, 100, 250, Side::Ask));
        assert!(detector.observe_obo(30, 7, cancel(1, 200)).is_none());

        let signal = detector
            .observe_obo(40, 7, qty_modify(2, 100))
            .expect("liquidity pull signal");
        assert_eq!(signal.instrument_id, 7);
        assert_eq!(signal.price, 100);
        assert_eq!(signal.pulled_side, Side::Ask);
        assert_eq!(signal.opposing_side, Side::Bid);
        assert_eq!(signal.pulled_qty, 350);
        assert_eq!(signal.executed_qty, 0);
        assert_eq!(signal.replenished_qty, 500);
        assert_eq!(signal.max_visible_qty, 500);
        assert_eq!(signal.visible_qty_after, 150);
        assert_eq!(signal.pull_events, 2);
        assert_eq!(signal.pull_ratio_bps, 7_000);
        assert_eq!(signal.execution_ratio_bps, 0);
        assert_eq!(signal.visible_after_ratio_bps, 3_000);
        assert!(signal.confidence_bps >= 8_000);
    }

    #[test]
    fn liquidity_pull_rejects_execution_only_liquidity_removal() {
        let mut config = liquidity_pull_cfg();
        config.max_visible_after_ratio_bps = 10_000;
        let mut detector = LiquidityPullDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 250, Side::Bid));
        detector.observe_obo(20, 7, add(2, 100, 250, Side::Bid));
        detector.observe_obo(30, 7, execute(1, 100, 250, Side::Ask));
        let signal = detector.observe_obo(40, 7, execute(2, 100, 250, Side::Ask));
        assert!(signal.is_none());
    }

    #[test]
    fn liquidity_pull_rejects_small_or_one_event_pull() {
        let mut detector = LiquidityPullDetector::new(liquidity_pull_cfg());
        detector.observe_obo(10, 7, add(1, 100, 500, Side::Ask));
        let one_event_signal = detector.observe_obo(20, 7, cancel(1, 300));
        assert!(one_event_signal.is_none());

        let mut detector = LiquidityPullDetector::new(liquidity_pull_cfg());
        detector.observe_obo(10, 7, add(1, 100, 250, Side::Ask));
        detector.observe_obo(20, 7, add(2, 100, 250, Side::Ask));
        detector.observe_obo(30, 7, cancel(1, 100));
        let small_signal = detector.observe_obo(40, 7, cancel(2, 100));
        assert!(small_signal.is_none());
    }

    #[test]
    fn liquidity_pull_duplicate_add_replacement_does_not_count_as_pull() {
        let mut config = liquidity_pull_cfg();
        config.min_pull_events = 1;
        config.min_pulled_qty = 200;
        let mut detector = LiquidityPullDetector::new(config);
        detector.observe_obo(10, 7, add(1, 100, 500, Side::Ask));
        let signal = detector.observe_obo(20, 7, add(1, 100, 250, Side::Ask));
        assert!(signal.is_none());
    }

    #[test]
    fn liquidity_pull_raw_frame_parser_feeds_detector() {
        let mut detector = LiquidityPullDetector::new(liquidity_pull_cfg());
        let add_one = add(1, 100, 250, Side::Ask);
        let add_two = add(2, 100, 250, Side::Ask);
        let cancel_one = cancel(1, 200);
        let modify_two = qty_modify(2, 100);

        let add_frame = raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_one));
        assert!(detector.observe_raw_frame(&add_frame).unwrap().is_none());
        let add_frame = raw_frame(msg_type::OBO_ADD, 7, 2, 11, 110, event_payload(&add_two));
        assert!(detector.observe_raw_frame(&add_frame).unwrap().is_none());
        let cancel_frame = raw_frame(
            msg_type::OBO_CANCEL,
            7,
            3,
            12,
            120,
            event_payload(&cancel_one),
        );
        assert!(detector.observe_raw_frame(&cancel_frame).unwrap().is_none());
        let modify_frame = raw_frame(
            msg_type::OBO_MODIFY,
            7,
            4,
            13,
            130,
            event_payload(&modify_two),
        );
        let signal = detector
            .observe_raw_frame(&modify_frame)
            .unwrap()
            .expect("raw frame liquidity pull signal");
        assert_eq!(signal.pulled_qty, 350);
        assert_eq!(signal.max_visible_qty, 500);
    }

    #[test]
    fn liquidity_pull_replay_validation_is_deterministic_and_dedupes_live_frames() {
        let add_one = add(1, 100, 250, Side::Ask);
        let add_two = add(2, 100, 250, Side::Ask);
        let cancel_one = cancel(1, 200);
        let modify_two = qty_modify(2, 100);
        let frames = vec![
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_one)),
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_one)),
            raw_frame(msg_type::OBO_ADD, 7, 2, 11, 110, event_payload(&add_two)),
            raw_frame(
                msg_type::OBO_CANCEL,
                7,
                3,
                12,
                120,
                event_payload(&cancel_one),
            ),
            raw_frame(
                msg_type::OBO_MODIFY,
                7,
                4,
                13,
                130,
                event_payload(&modify_two),
            ),
        ];

        let validation = validate_liquidity_pull_replay(&frames, liquidity_pull_cfg());
        assert!(validation.deterministic);
        assert_eq!(validation.first.frames_total, 5);
        assert_eq!(validation.first.parsed_events, 4);
        assert_eq!(validation.first.duplicate_events, 1);
        assert_eq!(validation.first.signals, 1);
        assert_ne!(validation.first.signal_hash, REPLAY_HASH_OFFSET);
        assert_eq!(validation.first.diagnostics.counts.total, 1);
        assert_eq!(validation.first.diagnostics.counts.liquidity_pull, 1);
        assert_eq!(validation.first.diagnostics.regime, SignalRegime::SpoofRisk);
        assert_eq!(
            validation.first.diagnostics.top_features[0].signal_kind,
            SignalKind::LiquidityPull
        );
        assert!(validation.first.diagnostics.score.avg_score_bps >= 8_000);
        assert_eq!(validation.first, validation.second);
    }

    #[test]
    fn participant_replay_reports_combined_session_diagnostics() {
        let add_one = add(1, 100, 250, Side::Ask);
        let add_two = add(2, 100, 250, Side::Ask);
        let cancel_one = cancel(1, 200);
        let modify_two = qty_modify(2, 100);
        let frames = vec![
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_one)),
            raw_frame(msg_type::OBO_ADD, 7, 1, 10, 100, event_payload(&add_one)),
            raw_frame(msg_type::OBO_ADD, 7, 2, 11, 110, event_payload(&add_two)),
            raw_frame(
                msg_type::OBO_CANCEL,
                7,
                3,
                12,
                120,
                event_payload(&cancel_one),
            ),
            raw_frame(
                msg_type::OBO_MODIFY,
                7,
                4,
                13,
                130,
                event_payload(&modify_two),
            ),
        ];

        let validation =
            validate_participant_replay(&frames, cfg(), iceberg_cfg(), liquidity_pull_cfg());
        assert!(validation.deterministic);
        assert_eq!(validation.first.frames_total, 5);
        assert_eq!(validation.first.parsed_events, 4);
        assert_eq!(validation.first.duplicate_events, 1);
        assert_eq!(validation.first.signals, 1);
        assert_eq!(validation.first.diagnostics.counts.total, 1);
        assert_eq!(validation.first.diagnostics.counts.liquidity_pull, 1);
        assert_eq!(validation.first.diagnostics.regime, SignalRegime::SpoofRisk);
        assert_eq!(validation.first.diagnostics.liquidity_pull_score.signals, 1);
        assert_ne!(validation.first.signal_hash, REPLAY_HASH_OFFSET);
        assert_eq!(validation.first, validation.second);
    }

    fn feature_cfg() -> MicrostructureFeatureConfig {
        MicrostructureFeatureConfig {
            depth_levels: 10,
            z_alpha: 0.5,
            z_min_samples: 1,
            z_clip: 20.0,
        }
    }

    fn parsed_feature_event(sequence: u64, ts_ns: u64, event: OboEventV1) -> ParsedOboEvent {
        ParsedOboEvent {
            instrument_id: 7,
            sequence,
            global_sequence: sequence,
            send_time_ns: ts_ns,
            event,
        }
    }

    fn outcome_snapshot(ts_ns: u64, mid_price_e8: Option<i64>) -> MicrostructureFeatureSnapshot {
        MicrostructureFeatureSnapshot {
            instrument_id: 7,
            sequence: ts_ns,
            global_sequence: ts_ns,
            ts_ns,
            wall_time_ns: None,
            best_bid_price_e8: mid_price_e8,
            best_bid_qty: 100,
            best_ask_price_e8: mid_price_e8,
            best_ask_qty: 100,
            mid_price_e8,
            spread_e8: Some(0),
            depth3_imb_z: 0.0,
            weighted_book_imb_z: 0.0,
            imbalance_l3_z: 0.0,
            ask_sz_l1_z: 0.0,
            touch_depth_ratio_bid_z: 0.0,
            cancel_touch_bid_qty_15s_z: 0.0,
            ofi_z: 0.0,
            trade_cnt_imb_300s_z: 0.0,
            trade_touch_buy_qty_15s_z: 0.0,
            trade_vol_120s_z: 0.0,
            trade_vol_300s_z: 0.0,
            mom_15s_z: 0.0,
            mom_60s_z: 0.0,
            slope_z: 0.0,
            dow_sin: 0.0,
            raw: MicrostructureFeatureRawValues {
                depth3_imb: 0.0,
                weighted_book_imb: 0.0,
                imbalance_l3: 0.0,
                ask_sz_l1: 0.0,
                touch_depth_ratio_bid: 0.0,
                cancel_touch_bid_qty_15s: 0.0,
                ofi_15s: 0.0,
                trade_cnt_imb_300s: 0.0,
                trade_touch_buy_qty_15s: 0.0,
                trade_vol_120s: 0.0,
                trade_vol_300s: 0.0,
                mom_15s: 0.0,
                mom_60s: 0.0,
                slope: 0.0,
                dow_sin: 0.0,
            },
        }
    }

    fn outcome_absorption_signal(ts_ns: u64, passive_side: Side) -> ParticipantSignal {
        ParticipantSignal::Absorption(AbsorptionSignal {
            instrument_id: 7,
            price: 100,
            passive_side,
            aggressor_side: opposite_side(passive_side),
            window_start_ns: ts_ns.saturating_sub(100),
            window_end_ns: ts_ns,
            executed_qty: 100,
            replenished_qty: 50,
            pulled_qty: 0,
            visible_qty_after: 100,
            execute_events: 2,
            replenish_events: 1,
            pull_events: 0,
            replenishment_ratio_bps: 5_000,
            pull_ratio_bps: 0,
            confidence_bps: 8_000,
        })
    }

    #[test]
    fn microstructure_features_track_depth_flow_and_touch_activity() {
        let mut engine = MicrostructureFeatureEngine::new(feature_cfg());
        let wall_time_ns = Some(1_704_067_200_000_000_000);
        engine.observe_parsed(
            &parsed_feature_event(0, 10, add(1, 100, 200, Side::Bid)),
            wall_time_ns,
        );
        let initial = engine
            .observe_parsed(
                &parsed_feature_event(0, 20, add(2, 101, 100, Side::Ask)),
                wall_time_ns,
            )
            .expect("initial feature snapshot");
        assert_eq!(initial.best_bid_qty, 200);
        assert_eq!(initial.best_ask_qty, 100);
        assert_eq!(initial.raw.ofi_15s, 0.0);
        assert!(initial.raw.depth3_imb > 0.0);

        let trade = engine
            .observe_parsed(
                &parsed_feature_event(1, 30, execute(2, 101, 40, Side::Bid)),
                wall_time_ns,
            )
            .expect("trade feature snapshot");
        assert_eq!(trade.best_ask_qty, 60);
        assert_eq!(trade.raw.trade_vol_120s, 40.0);
        assert_eq!(trade.raw.trade_vol_300s, 40.0);
        assert_eq!(trade.raw.trade_cnt_imb_300s, 1.0);
        assert_eq!(trade.raw.trade_touch_buy_qty_15s, 40.0);
        assert_eq!(trade.raw.ofi_15s, 40.0);
        assert!(trade.ofi_z.is_finite());

        let pull = engine
            .observe_parsed(&parsed_feature_event(2, 40, cancel(1, 50)), wall_time_ns)
            .expect("cancel feature snapshot");
        assert_eq!(pull.best_bid_qty, 150);
        assert_eq!(pull.raw.cancel_touch_bid_qty_15s, 50.0);
        assert_eq!(pull.raw.ofi_15s, -10.0);
    }

    #[test]
    fn microstructure_features_do_not_count_snapshot_adds_as_flow() {
        let mut engine = MicrostructureFeatureEngine::new(feature_cfg());
        let snapshot = engine
            .observe_parsed(
                &parsed_feature_event(0, 10, add(1, 100, 250, Side::Bid)),
                None,
            )
            .expect("snapshot add feature");
        assert_eq!(snapshot.raw.ofi_15s, 0.0);
        assert_eq!(snapshot.raw.trade_vol_120s, 0.0);
        assert_eq!(snapshot.raw.cancel_touch_bid_qty_15s, 0.0);

        let live = engine
            .observe_parsed(
                &parsed_feature_event(1, 20, add(2, 99, 50, Side::Bid)),
                None,
            )
            .expect("live add feature");
        assert_eq!(live.raw.ofi_15s, 50.0);
        assert_eq!(live.best_bid_qty, 250);
    }

    #[test]
    fn microstructure_features_clamp_out_of_order_timestamps() {
        let mut engine = MicrostructureFeatureEngine::new(feature_cfg());
        let first = engine
            .observe_parsed(
                &parsed_feature_event(1, 20, add(1, 100, 100, Side::Bid)),
                None,
            )
            .expect("first feature snapshot");
        let late = engine
            .observe_parsed(
                &parsed_feature_event(2, 10, add(2, 99, 25, Side::Bid)),
                None,
            )
            .expect("late feature snapshot");

        assert_eq!(first.ts_ns, 20);
        assert_eq!(late.ts_ns, 20);
        assert_eq!(late.raw.ofi_15s, 125.0);
    }

    #[test]
    fn signal_diagnostics_top_features_use_stable_tie_breakers() {
        let mut diagnostics = SignalDiagnosticsAccumulator::default();
        for signal_kind in [
            SignalKind::LiquidityPull,
            SignalKind::Iceberg,
            SignalKind::Absorption,
        ] {
            diagnostics.feature_totals.insert(
                SignalFeatureKey {
                    signal_kind,
                    feature: "shared_feature",
                },
                FeatureScoreAccumulator {
                    observations: 1,
                    score_sum_bps: 1_000,
                    contribution_sum_bps: 100,
                    weight_pct: 10,
                },
            );
        }

        let features = diagnostics.top_features(8);
        let kinds = features
            .iter()
            .map(|feature| feature.signal_kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                SignalKind::Absorption,
                SignalKind::Iceberg,
                SignalKind::LiquidityPull
            ]
        );
    }

    #[test]
    fn signal_outcomes_settle_directional_markouts_by_horizon() {
        let mut tracker = SignalOutcomeTracker::with_max_pending(8);
        let signal = outcome_absorption_signal(100, Side::Bid);
        assert!(tracker.track_signal(&signal, &outcome_snapshot(100, Some(1_000))));

        tracker.observe_snapshot(&outcome_snapshot(100 + NS_PER_SECOND - 1, Some(1_100)));
        let pending = tracker.snapshot();
        assert_eq!(pending.tracked_signals, 1);
        assert_eq!(pending.pending_signals, 1);
        assert!(pending.rows.is_empty());

        tracker.observe_snapshot(&outcome_snapshot(100 + NS_PER_SECOND, Some(1_125)));
        let one_second = tracker.snapshot();
        assert_eq!(one_second.pending_signals, 1);
        assert_eq!(one_second.rows.len(), 1);
        assert_eq!(one_second.rows[0].horizon_ns, NS_PER_SECOND);
        assert_eq!(one_second.rows[0].favorable, 1);
        assert_eq!(one_second.rows[0].avg_signed_markout_e8, 125);

        tracker.observe_snapshot(&outcome_snapshot(100 + 5 * NS_PER_SECOND, Some(980)));
        tracker.observe_snapshot(&outcome_snapshot(100 + 30 * NS_PER_SECOND, Some(1_000)));
        let settled = tracker.snapshot();
        assert_eq!(settled.pending_signals, 0);
        assert_eq!(settled.rows.len(), 3);
        assert_eq!(settled.rows[1].horizon_ns, 5 * NS_PER_SECOND);
        assert_eq!(settled.rows[1].adverse, 1);
        assert_eq!(settled.rows[1].avg_signed_markout_e8, -20);
        assert_eq!(settled.rows[2].horizon_ns, 30 * NS_PER_SECOND);
        assert_eq!(settled.rows[2].flat, 1);
        assert_eq!(settled.rows[2].avg_signed_markout_e8, 0);
    }

    #[test]
    fn signal_outcomes_flip_direction_for_ask_absorption() {
        let mut tracker = SignalOutcomeTracker::with_max_pending(8);
        let signal = outcome_absorption_signal(100, Side::Ask);
        assert!(tracker.track_signal(&signal, &outcome_snapshot(100, Some(1_000))));

        tracker.observe_snapshot(&outcome_snapshot(100 + NS_PER_SECOND, Some(950)));
        let summary = tracker.snapshot();
        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].favorable, 1);
        assert_eq!(summary.rows[0].avg_signed_markout_e8, 50);
    }
}
