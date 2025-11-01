//  decoder for EMDI-style messages. FAST-like 
//  supports a compact varint framing
// sufficient to drive the engine: Add/Mod/Del/Trade events.
// Framing per message (repeated in UDP payload):
//   [pmap: 1..N bytes, MSB=1 continuation, MSB=0 last, 7 bits per byte]
//   [template_id: stop-bit int]
//   [body_len: stop-bit int]  -- number of bytes in the message body
//   [body fields encoded as stop-bit integers and small fixed fields]
// Templates:
//   1: Add { order_id(u64 sbi), instr(u32 sbi), side(u8 raw), price(i64 zigzag), qty(i64 zigzag) }
//   2: Mod { order_id(u64 sbi), qty(i64 zigzag) }
//   3: Del { order_id(u64 sbi) }
//   4: Trade { instr(u32 sbi), price(i64 zigzag), qty(i64 zigzag), maker_order_id(u64 sbi, optional via pmap bit0), taker_side(u8 raw, optional pmap bit1) }

use crate::parser::{Event, MessageDecoder, Side};

#[derive(Default)]
pub struct FastEmdiDecoder;

impl FastEmdiDecoder { pub fn new() -> Self { Self } }

impl MessageDecoder for FastEmdiDecoder {
    #[inline]
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>) {
        let mut off = 0usize;
        while off < payload.len() {
            let (pmap, n) = read_pmap(payload, off);
            if n == 0 { break; }
            off += n;
            let (tmpl, n2) = read_sbi_u64(payload, off);
            if n2 == 0 { break; }
            off += n2;
            let (body_len, n3) = read_sbi_u64(payload, off);
            if n3 == 0 { break; }
            off += n3;
            if off + (body_len as usize) > payload.len() { break; }
            let body = &payload[off..off + (body_len as usize)];
            off += body_len as usize;

            match tmpl {
                1 => on_add(body, out),
                2 => on_mod(body, out),
                3 => on_del(body, out),
                4 => on_trade(body, out, pmap),
                _ => { /* skip unknown */ }
            }
        }
    }
}

#[inline]
fn read_pmap(b: &[u8], mut off: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    let mut consumed = 0usize;
    while off < b.len() {
        let byte = b[off];
        off += 1;
        consumed += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 { break; }
        shift += 7;
        if shift > 56 { break; }
    }
    (v, consumed)
}

#[inline]
fn read_sbi_u64(b: &[u8], mut off: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    let mut consumed = 0usize;
    while off < b.len() {
        let byte = b[off];
        off += 1;
        consumed += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 { break; }
        shift += 7;
        if shift > 63 { break; }
    }
    (v, consumed)
}

#[inline]
fn read_sbi_i64(b: &[u8], off: usize) -> (i64, usize) {
    let (uv, n) = read_sbi_u64(b, off);
    (zigzag_decode(uv), n)
}

#[inline]
fn zigzag_decode(u: u64) -> i64 { ((u >> 1) as i64) ^ (-((u & 1) as i64)) }

#[inline]
fn on_add(body: &[u8], out: &mut Vec<Event>) {
    let mut o = 0usize;
    let (order_id, n1) = read_sbi_u64(body, o); o += n1; if n1 == 0 { return; }
    let (instr, n2) = read_sbi_u64(body, o); o += n2; if n2 == 0 { return; }
    if o >= body.len() { return; }
    let side = if body[o] == 0 { Side::Bid } else { Side::Ask }; o += 1;
    let (px, n3) = read_sbi_i64(body, o); o += n3; if n3 == 0 { return; }
    let (qty, n4) = read_sbi_i64(body, o); if n4 == 0 { return; }
    out.push(Event::Add { order_id, instr: instr as u32, px, qty, side });
}

#[inline]
fn on_mod(body: &[u8], out: &mut Vec<Event>) {
    let mut o = 0usize;
    let (order_id, n1) = read_sbi_u64(body, o); o += n1; if n1 == 0 { return; }
    let (qty, _n2) = read_sbi_i64(body, o);
    out.push(Event::Mod { order_id, qty });
}

#[inline]
fn on_del(body: &[u8], out: &mut Vec<Event>) {
    let (order_id, _n1) = read_sbi_u64(body, 0);
    out.push(Event::Del { order_id });
}

#[inline]
fn on_trade(body: &[u8], out: &mut Vec<Event>, pmap: u64) {
    let mut o = 0usize;
    let (instr, n1) = read_sbi_u64(body, o); o += n1; if n1 == 0 { return; }
    let (px, n2) = read_sbi_i64(body, o); o += n2; if n2 == 0 { return; }
    let (qty, n3) = read_sbi_i64(body, o); o += n3; if n3 == 0 { return; }
    let mut maker_order_id = None;
    if pmap & 0x1 != 0 {
        let (oid, n4) = read_sbi_u64(body, o); o += n4; if n4 == 0 { return; }
        maker_order_id = Some(oid);
    }
    let mut taker_side = None;
    if pmap & 0x2 != 0 {
        if o < body.len() { taker_side = Some(if body[o] == 0 { Side::Bid } else { Side::Ask }); }
    }
    out.push(Event::Trade { instr: instr as u32, px, qty, maker_order_id, taker_side });
}


