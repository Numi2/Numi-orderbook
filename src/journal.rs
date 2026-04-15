use crate::orderbook::OrderBook;
use crate::parser::{Event, Side, VenueState};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

const MAX_RECORD_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub seq: u64,
    pub event_index: u16,
    pub event: JournalEvent,
    pub state_hash_after: Option<u64>,
}

impl JournalRecord {
    pub fn new(seq: u64, event: &Event, state_hash_after: Option<u64>) -> Self {
        Self::new_at(seq, 0, event, state_hash_after)
    }

    pub fn new_at(
        seq: u64,
        event_index: u16,
        event: &Event,
        state_hash_after: Option<u64>,
    ) -> Self {
        Self {
            seq,
            event_index,
            event: JournalEvent::from(event),
            state_hash_after,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEvent {
    Add {
        order_id: u64,
        instr: u32,
        px: i64,
        qty: i64,
        side: Side,
    },
    Mod {
        order_id: u64,
        qty: i64,
    },
    Del {
        order_id: u64,
    },
    MassDel {
        instr: u32,
    },
    Execute {
        instr: u32,
        px: i64,
        qty: i64,
        order_id: u64,
        taker_side: Option<Side>,
        match_id: u32,
        full: bool,
    },
    Trade {
        instr: u32,
        px: i64,
        qty: i64,
        maker_order_id: Option<u64>,
        taker_side: Option<Side>,
    },
    State {
        template_id: u16,
        msg_seq_num: u32,
        instr: Option<u32>,
        state: VenueState,
    },
    SequenceGap {
        expected: u32,
        got: u32,
    },
    Heartbeat,
}

impl From<&Event> for JournalEvent {
    fn from(event: &Event) -> Self {
        match *event {
            Event::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            } => Self::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            },
            Event::Mod { order_id, qty } => Self::Mod { order_id, qty },
            Event::Del { order_id } => Self::Del { order_id },
            Event::MassDel { instr } => Self::MassDel { instr },
            Event::Execute {
                instr,
                px,
                qty,
                order_id,
                taker_side,
                match_id,
                full,
            } => Self::Execute {
                instr,
                px,
                qty,
                order_id,
                taker_side,
                match_id,
                full,
            },
            Event::Trade {
                instr,
                px,
                qty,
                maker_order_id,
                taker_side,
            } => Self::Trade {
                instr,
                px,
                qty,
                maker_order_id,
                taker_side,
            },
            Event::State {
                template_id,
                msg_seq_num,
                instr,
                state,
            } => Self::State {
                template_id,
                msg_seq_num,
                instr,
                state,
            },
            Event::SequenceGap { expected, got } => Self::SequenceGap { expected, got },
            Event::Heartbeat => Self::Heartbeat,
        }
    }
}

impl From<JournalEvent> for Event {
    fn from(event: JournalEvent) -> Self {
        match event {
            JournalEvent::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            } => Self::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            },
            JournalEvent::Mod { order_id, qty } => Self::Mod { order_id, qty },
            JournalEvent::Del { order_id } => Self::Del { order_id },
            JournalEvent::MassDel { instr } => Self::MassDel { instr },
            JournalEvent::Execute {
                instr,
                px,
                qty,
                order_id,
                taker_side,
                match_id,
                full,
            } => Self::Execute {
                instr,
                px,
                qty,
                order_id,
                taker_side,
                match_id,
                full,
            },
            JournalEvent::Trade {
                instr,
                px,
                qty,
                maker_order_id,
                taker_side,
            } => Self::Trade {
                instr,
                px,
                qty,
                maker_order_id,
                taker_side,
            },
            JournalEvent::State {
                template_id,
                msg_seq_num,
                instr,
                state,
            } => Self::State {
                template_id,
                msg_seq_num,
                instr,
                state,
            },
            JournalEvent::SequenceGap { expected, got } => Self::SequenceGap { expected, got },
            JournalEvent::Heartbeat => Self::Heartbeat,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplayReport {
    pub events: usize,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub non_monotonic_sequences: usize,
    pub final_hash: u64,
    pub expected_hash: Option<u64>,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRestartReport {
    pub snapshot_hash: u64,
    pub anchored: bool,
    pub anchor_seq: Option<u64>,
    pub anchor_event_index: Option<u16>,
    pub skipped_records: usize,
    pub replay: JournalReplayReport,
    pub matched: bool,
}

pub fn encode_record(record: &JournalRecord) -> Result<Vec<u8>> {
    bincode::serialize(record).context("encode journal record")
}

pub fn decode_record(bytes: &[u8]) -> Result<JournalRecord> {
    bincode::deserialize(bytes).context("decode journal record")
}

pub fn append_record(writer: &mut impl Write, record: &JournalRecord) -> Result<()> {
    let bytes = encode_record(record)?;
    let len = u32::try_from(bytes.len()).context("journal record too large")?;
    if len > MAX_RECORD_BYTES {
        anyhow::bail!("journal record exceeds max frame size: {len}");
    }
    writer
        .write_all(&len.to_be_bytes())
        .context("write journal record length")?;
    writer
        .write_all(&bytes)
        .context("write journal record payload")
}

pub fn read_record(reader: &mut impl Read) -> Result<Option<JournalRecord>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf[..1]) {
        Ok(()) => {
            reader
                .read_exact(&mut len_buf[1..])
                .context("read partial journal record length")?;
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read journal record length"),
    }

    let len = u32::from_be_bytes(len_buf);
    if len > MAX_RECORD_BYTES {
        anyhow::bail!("journal record exceeds max frame size: {len}");
    }
    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .context("read journal record payload")?;
    decode_record(&payload).map(Some)
}

pub fn replay_records<I>(records: I, book: &mut OrderBook) -> JournalReplayReport
where
    I: IntoIterator<Item = JournalRecord>,
{
    let mut replay = ReplayState::default();
    for record in records {
        replay.apply(record, book);
    }
    replay.finish(book)
}

pub fn replay_reader(reader: &mut impl Read, book: &mut OrderBook) -> Result<JournalReplayReport> {
    let mut replay = ReplayState::default();
    while let Some(record) = read_record(reader)? {
        replay.apply(record, book);
    }
    Ok(replay.finish(book))
}

pub fn replay_after_snapshot(
    reader: &mut impl Read,
    book: &mut OrderBook,
) -> Result<JournalRestartReport> {
    let snapshot_hash = book.state_hash();
    let mut skipped_records = 0usize;
    let mut anchor = None;

    while let Some(record) = read_record(reader)? {
        let key = (record.seq, record.event_index);
        skipped_records += 1;
        if record.state_hash_after == Some(snapshot_hash) {
            anchor = Some(key);
            break;
        }
    }

    let Some(anchor_key) = anchor else {
        let replay = ReplayState::default().finish(book);
        return Ok(JournalRestartReport {
            snapshot_hash,
            anchored: skipped_records == 0,
            anchor_seq: None,
            anchor_event_index: None,
            skipped_records,
            matched: skipped_records == 0 && replay.matched,
            replay,
        });
    };

    let mut replay = ReplayState::after_key(anchor_key);
    while let Some(record) = read_record(reader)? {
        replay.apply(record, book);
    }
    let replay = replay.finish(book);
    Ok(JournalRestartReport {
        snapshot_hash,
        anchored: true,
        anchor_seq: Some(anchor_key.0),
        anchor_event_index: Some(anchor_key.1),
        skipped_records,
        matched: replay.matched,
        replay,
    })
}

#[derive(Debug, Default)]
struct ReplayState {
    events: usize,
    first_seq: Option<u64>,
    last_key: Option<(u64, u16)>,
    non_monotonic_sequences: usize,
    expected_hash: Option<u64>,
}

impl ReplayState {
    fn after_key(key: (u64, u16)) -> Self {
        Self {
            last_key: Some(key),
            ..Self::default()
        }
    }

    fn apply(&mut self, record: JournalRecord, book: &mut OrderBook) {
        if self.first_seq.is_none() {
            self.first_seq = Some(record.seq);
        }
        let key = (record.seq, record.event_index);
        if let Some(prev) = self.last_key {
            if key <= prev {
                self.non_monotonic_sequences += 1;
            }
        }
        self.last_key = Some(key);

        let event = Event::from(record.event);
        book.apply(&event);
        self.events += 1;
        if let Some(hash) = record.state_hash_after {
            self.expected_hash = Some(hash);
        }
    }

    fn finish(self, book: &OrderBook) -> JournalReplayReport {
        let final_hash = book.state_hash();
        let hash_matches = self
            .expected_hash
            .map(|expected| expected == final_hash)
            .unwrap_or(true);
        JournalReplayReport {
            events: self.events,
            first_seq: self.first_seq,
            last_seq: self.last_key.map(|(seq, _)| seq),
            non_monotonic_sequences: self.non_monotonic_sequences,
            final_hash,
            expected_hash: self.expected_hash,
            matched: self.non_monotonic_sequences == 0 && hash_matches,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn journal_roundtrips_records() {
        let event = Event::Trade {
            instr: 7,
            px: 10_001,
            qty: 25,
            maker_order_id: Some(42),
            taker_side: Some(Side::Ask),
        };
        let record = JournalRecord::new(99, &event, Some(1234));

        let encoded = encode_record(&record).unwrap();
        assert_eq!(decode_record(&encoded).unwrap(), record);

        let mut framed = Vec::new();
        append_record(&mut framed, &record).unwrap();
        let mut cursor = Cursor::new(framed);
        assert_eq!(read_record(&mut cursor).unwrap(), Some(record));
        assert_eq!(read_record(&mut cursor).unwrap(), None);
    }

    #[test]
    fn replay_matches_recorded_hash() {
        let events = [
            Event::Add {
                order_id: 1,
                instr: 10,
                px: 100,
                qty: 5,
                side: Side::Bid,
            },
            Event::Add {
                order_id: 2,
                instr: 10,
                px: 101,
                qty: 7,
                side: Side::Ask,
            },
            Event::Mod {
                order_id: 1,
                qty: 6,
            },
            Event::Del { order_id: 2 },
        ];

        let mut live = OrderBook::new(5);
        let mut records = Vec::new();
        for (idx, event) in events.iter().enumerate() {
            live.apply(event);
            records.push(JournalRecord::new(
                idx as u64 + 1,
                event,
                Some(live.state_hash()),
            ));
        }

        let mut replayed = OrderBook::new(5);
        let report = replay_records(records, &mut replayed);
        assert_eq!(report.events, events.len());
        assert_eq!(report.expected_hash, Some(live.state_hash()));
        assert_eq!(report.final_hash, live.state_hash());
        assert!(report.matched);
    }

    #[test]
    fn replay_reader_streams_framed_records() {
        let events = [
            Event::Add {
                order_id: 1,
                instr: 10,
                px: 100,
                qty: 5,
                side: Side::Bid,
            },
            Event::Mod {
                order_id: 1,
                qty: 9,
            },
        ];

        let mut live = OrderBook::new(5);
        let mut framed = Vec::new();
        for (idx, event) in events.iter().enumerate() {
            live.apply(event);
            append_record(
                &mut framed,
                &JournalRecord::new_at(50, idx as u16, event, Some(live.state_hash())),
            )
            .unwrap();
        }

        let mut replayed = OrderBook::new(5);
        let mut cursor = Cursor::new(framed);
        let report = replay_reader(&mut cursor, &mut replayed).unwrap();
        assert_eq!(report.events, 2);
        assert_eq!(report.first_seq, Some(50));
        assert_eq!(report.last_seq, Some(50));
        assert!(report.matched);
    }

    #[test]
    fn replay_after_snapshot_anchors_on_recorded_hash() {
        let events = [
            Event::Add {
                order_id: 1,
                instr: 10,
                px: 100,
                qty: 5,
                side: Side::Bid,
            },
            Event::Add {
                order_id: 2,
                instr: 10,
                px: 101,
                qty: 3,
                side: Side::Ask,
            },
            Event::Mod {
                order_id: 1,
                qty: 9,
            },
        ];

        let mut live = OrderBook::new(5);
        let mut framed = Vec::new();
        let mut snapshot = None;
        for (idx, event) in events.iter().enumerate() {
            live.apply(event);
            if idx == 1 {
                snapshot = Some(OrderBook::from_export(live.export()));
            }
            append_record(
                &mut framed,
                &JournalRecord::new_at(10 + idx as u64, 0, event, Some(live.state_hash())),
            )
            .unwrap();
        }

        let mut restored = snapshot.unwrap();
        let mut cursor = Cursor::new(framed);
        let report = replay_after_snapshot(&mut cursor, &mut restored).unwrap();
        assert!(report.anchored);
        assert_eq!(report.anchor_seq, Some(11));
        assert_eq!(report.skipped_records, 2);
        assert_eq!(report.replay.events, 1);
        assert_eq!(restored.state_hash(), live.state_hash());
        assert!(report.matched);
    }

    #[test]
    fn replay_after_snapshot_reports_missing_anchor() {
        let mut framed = Vec::new();
        append_record(
            &mut framed,
            &JournalRecord::new_at(1, 0, &Event::Heartbeat, Some(999)),
        )
        .unwrap();

        let mut book = OrderBook::new(1);
        let mut cursor = Cursor::new(framed);
        let report = replay_after_snapshot(&mut cursor, &mut book).unwrap();
        assert!(!report.anchored);
        assert!(!report.matched);
        assert_eq!(report.skipped_records, 1);
        assert_eq!(report.replay.events, 0);
    }

    #[test]
    fn replay_flags_non_monotonic_sequences() {
        let records = vec![
            JournalRecord::new(2, &Event::Heartbeat, None),
            JournalRecord::new(2, &Event::Heartbeat, None),
        ];
        let mut book = OrderBook::new(1);
        let report = replay_records(records, &mut book);
        assert_eq!(report.non_monotonic_sequences, 1);
        assert!(!report.matched);
    }
}
