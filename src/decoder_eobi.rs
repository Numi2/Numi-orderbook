// src/decoder_eobi.rs
//
// Minimal, allocation-free EOBI/SBE-like decoder that maps venue messages
// to the engine's Event model. This is not a full Eurex spec, but follows
// SBE framing and common order-flow templates. Hot-path does zero heap allocs.
//
// Frame layout per message (little-endian):
//   [block_len: u16][template_id: u16][schema_id: u16][version: u16][body...]
//
// Supported template_id values (example mapping):
//   1001: Add Order
//     body: order_id(u64) instr(u32) side(u8 0=bid,1=ask) price(i64) qty(i64)
//   1002: Modify Order (absolute qty)
//     body: order_id(u64) qty(i64)
//   1003: Delete Order
//     body: order_id(u64)
//   1004: Trade
//     body: instr(u32) price(i64) qty(i64) maker_order_id(u64) taker_side(u8 0/1, 255=unknown)
// Unknown templates are skipped safely.
//
use crate::parser::{Event, MessageDecoder, Side};

#[derive(Default)]
pub struct EobiSbeDecoder;

impl EobiSbeDecoder {
    pub fn new() -> Self { Self }
}

impl MessageDecoder for EobiSbeDecoder {
    #[inline]
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>) {
        let mut off = 0usize;
        while off + 8 <= payload.len() {
            let block_len = le_u16(&payload[off..off + 2]) as usize; off += 2;
            let template_id = le_u16(&payload[off..off + 2]); off += 2;
            let _schema_id  = le_u16(&payload[off..off + 2]); off += 2;
            let _version    = le_u16(&payload[off..off + 2]); off += 2;

            if off + block_len > payload.len() { break; }
            let body = &payload[off..off + block_len];
            off += block_len;

            match template_id {
                1001 => decode_add(body, out),
                1002 => decode_mod(body, out),
                1003 => decode_del(body, out),
                1004 => decode_trade(body, out),
                _ => { /* skip unknown template */ }
            }
        }
    }
}

#[inline] fn le_u16(b: &[u8]) -> u16 { u16::from_le_bytes([b[0], b[1]]) }
#[inline] fn le_u32(b: &[u8]) -> u32 { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) }
#[inline] fn le_u64(b: &[u8]) -> u64 { u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) }
#[inline] fn le_i64(b: &[u8]) -> i64 { i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) }

#[inline]
fn decode_add(body: &[u8], out: &mut Vec<Event>) {
    if body.len() < 8 + 4 + 1 + 8 + 8 { return; }
    let mut o = 0usize;
    let order_id = le_u64(&body[o..o+8]); o += 8;
    let instr    = le_u32(&body[o..o+4]); o += 4;
    let side     = match body[o] { 0 => Side::Bid, _ => Side::Ask }; o += 1;
    let px       = le_i64(&body[o..o+8]); o += 8;
    let qty      = le_i64(&body[o..o+8]);
    out.push(Event::Add { order_id, instr, px, qty, side });
}

#[inline]
fn decode_mod(body: &[u8], out: &mut Vec<Event>) {
    if body.len() < 8 + 8 { return; }
    let mut o = 0usize;
    let order_id = le_u64(&body[o..o+8]); o += 8;
    let qty      = le_i64(&body[o..o+8]);
    out.push(Event::Mod { order_id, qty });
}

#[inline]
fn decode_del(body: &[u8], out: &mut Vec<Event>) {
    if body.len() < 8 { return; }
    let order_id = le_u64(&body[0..8]);
    out.push(Event::Del { order_id });
}

#[inline]
fn decode_trade(body: &[u8], out: &mut Vec<Event>) {
    if body.len() < 4 + 8 + 8 + 8 + 1 { return; }
    let mut o = 0usize;
    let instr = le_u32(&body[o..o+4]); o += 4;
    let px    = le_i64(&body[o..o+8]); o += 8;
    let qty   = le_i64(&body[o..o+8]); o += 8;
    let maker_order_id = le_u64(&body[o..o+8]); o += 8;
    let taker_side = match body[o] { 0 => Some(Side::Bid), 1 => Some(Side::Ask), _ => None };
    out.push(Event::Trade { instr, px, qty, maker_order_id: Some(maker_order_id), taker_side });
}


