// src/parser.rs (patched: add serde derives for Side)
use crate::config::{Endian, ParserKind};
use crate::decoder_itch::Itch50Decoder;
use crate::decoder_eobi::EobiSbeDecoder;
use crate::decoder_fast::FastEmdiDecoder;
use std::sync::Arc;
use serde::{Serialize, Deserialize};

#[derive(Clone)]
pub struct SeqCfg {
    pub offset: u16,
    pub length: u8, // 4 or 8
    pub endian: Endian,
}

pub trait SeqExtractor: Send + Sync + 'static {
    fn extract_seq(&self, pkt: &[u8]) -> Option<u64>;
}

pub trait MessageDecoder: Send + Sync + 'static {
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side { Bid, Ask }

#[derive(Debug, Clone)]
pub enum Event {
    Add {
        order_id: u64,
        instr: u32,
        px: i64,
        qty: i64,
        side: Side,
    },
    Mod { order_id: u64, qty: i64 },
    Del { order_id: u64 },
    Trade {
        instr: u32,
        px: i64,
        qty: i64,
        maker_order_id: Option<u64>,
        taker_side: Option<Side>,
    },
    Heartbeat,
}

#[derive(Clone)]
pub struct Parser {
    seq: Arc<dyn SeqExtractor>,
    dec: Arc<dyn MessageDecoder>,
    pub max_messages_per_packet: usize,
}

impl Parser {
    pub fn seq_extractor(&self) -> Arc<dyn SeqExtractor> { self.seq.clone() }
    pub fn decoder(&self) -> Arc<dyn MessageDecoder> { self.dec.clone() }
}

pub fn build_parser(kind: ParserKind, seq: SeqCfg, max_per_packet: usize) -> anyhow::Result<Parser> {
    let seq_impl: Arc<dyn SeqExtractor> = Arc::new(FixedSeq { cfg: seq.clone() });

    let dec_impl: Arc<dyn MessageDecoder> = match kind {
        ParserKind::FixedBinary => Arc::new(EobiSbeDecoder::new()),
        ParserKind::FastLike => Arc::new(FastEmdiDecoder::new()),
        ParserKind::Itch50 => Arc::new(Itch50Decoder::new()),
    };

    Ok(Parser {
        seq: seq_impl,
        dec: dec_impl,
        max_messages_per_packet: max_per_packet.max(1),
    })
}

struct FixedSeq { cfg: SeqCfg }

impl SeqExtractor for FixedSeq {
    #[inline]
    fn extract_seq(&self, pkt: &[u8]) -> Option<u64> {
        let off = self.cfg.offset as usize;
        if pkt.len() < off + (self.cfg.length as usize) {
            return None;
        }
        match (self.cfg.length, &self.cfg.endian) {
            (8, Endian::Be) => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&pkt[off..off+8]);
                Some(u64::from_be_bytes(b))
            }
            (8, Endian::Le) => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&pkt[off..off+8]);
                Some(u64::from_le_bytes(b))
            }
            (4, Endian::Be) => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&pkt[off..off+4]);
                Some(u32::from_be_bytes(b) as u64)
            }
            (4, Endian::Le) => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&pkt[off..off+4]);
                Some(u32::from_le_bytes(b) as u64)
            }
            _ => None,
        }
    }
}

// FixedBinaryDecoder was a synthetic format used for bring-up. It has been
// replaced by a real EOBI/SBE-like implementation in `decoder_eobi.rs`.

// FastLike is implemented by FastEmdiDecoder in decoder_fast.rs