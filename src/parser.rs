// src/parser.rs 
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

#[allow(dead_code)]
pub trait MessageDecoder: Send + Sync + 'static {
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side { Bid, Ask }

#[allow(dead_code)]
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
    dec: DecoderImpl,
    pub max_messages_per_packet: usize,
}

#[derive(Clone)]
enum DecoderImpl {
    Fixed(EobiSbeDecoder), // FixedBinary -> EOBI/SBE-like
    Fast(FastEmdiDecoder),
    Itch(Itch50Decoder),
}

impl DecoderImpl {
    #[inline]
    fn decode(&self, payload: &[u8], out: &mut Vec<Event>) {
        match self {
            DecoderImpl::Fixed(d) => d.decode_messages(payload, out),
            DecoderImpl::Fast(d) => d.decode_messages(payload, out),
            DecoderImpl::Itch(d) => d.decode_messages(payload, out),
        }
    }
}

impl Parser {
    #[inline]
    pub fn seq_extractor(&self) -> Arc<dyn SeqExtractor> { self.seq.clone() }
    #[inline]
    pub fn decode_into(&self, payload: &[u8], out: &mut Vec<Event>) { self.dec.decode(payload, out) }
}

pub fn build_parser(kind: ParserKind, seq: SeqCfg, max_per_packet: usize) -> anyhow::Result<Parser> {
    let seq_impl: Arc<dyn SeqExtractor> = Arc::new(FixedSeq { cfg: seq.clone() });

    let dec_impl: DecoderImpl = match kind {
        ParserKind::FixedBinary => DecoderImpl::Fixed(EobiSbeDecoder::new()),
        ParserKind::FastLike => DecoderImpl::Fast(FastEmdiDecoder::new()),
        ParserKind::Itch50 => DecoderImpl::Itch(Itch50Decoder::new()),
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