// src/decoder_itch.rs
// Numan Thabit - decoder_itch.rs: NASDAQ ITCH 5.0 style; big-endian [len][type][body]; stateful (keeps an order map) to emit absolute quantities and enrich trades.
// Production‑quality, stateful decoder for a real‑world binary protocol: NASDAQ TotalView‑ITCH 5.0 style.
// It parses a UDP payload containing concatenated ITCH messages:
// [u16 big-endian length][u8 type][body bytes...], repeated.
//
// Supported messages (enough to drive a full order‑by‑order book):
//  - 'A' Add Order (no attribution)
//  - 'F' Add Order with MPID attribution (MPID ignored here)
//  - 'E' Order Executed
//  - 'C' Order Executed With Price (treated same as 'E' for book effect)
//  - 'X' Order Cancel (reduce shares)
//  - 'D' Order Delete (remove order)
//  - 'U' Order Replace (delete old, add new with new id/price/qty)
//  - 'P' Trade (non-cross) — treated as execution against a displayed order
//  - 'R' Stock Directory (optional; we simply accept it to avoid warnings)
// Unknown types are safely skipped.

use crate::parser::{Event, MessageDecoder, Side};
use hashbrown::HashMap;
use std::cell::UnsafeCell;

pub struct Itch50Decoder {
    // Decoder is used by a single decode thread; we avoid mutex overhead.
    inner: UnsafeCell<Inner>,
}

// Safety: We only use this decoder from a single thread (decode thread). We still
// need to satisfy trait bounds (Send + Sync) because the parser is cloned across
// threads but the decoder is not used there.
unsafe impl Send for Itch50Decoder {}
unsafe impl Sync for Itch50Decoder {}

#[derive(Default)]
struct Inner {
    /// order_ref -> state
    orders: HashMap<u64, OrderState>,
    /// optional: stock locate -> (last seen 8-byte symbol). Not required for book logic.
    last_symbol_by_locate: HashMap<u16, [u8; 8]>,
}

#[derive(Clone, Copy)]
struct OrderState {
    instr: u32, // Stock Locate widened
    qty: i64,
    px: i64, // price in 1/10000
    side: Side,
}

impl Itch50Decoder {
    pub fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner::default()),
        }
    }
}

impl Default for Itch50Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Itch50Decoder {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl MessageDecoder for Itch50Decoder {
    #[inline]
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>) {
        let mut off = 0usize;
        let st: &mut Inner = unsafe { &mut *self.inner.get() };

        while off + 3 <= payload.len() {
            let msg_len = be_u16(&payload[off..off + 2]) as usize;
            if msg_len < 1 {
                // length must at least contain message type
                break;
            }
            off += 2;
            if off + msg_len > payload.len() {
                // Truncated packet (drop tail gracefully)
                break;
            }

            let typ = payload[off] as char;
            off += 1;

            let body = &payload[off..off + (msg_len - 1)];
            off += msg_len - 1;

            match typ {
                'A' => on_add(body, st, out, /*with_mpid*/ false),
                'F' => on_add(body, st, out, /*with_mpid*/ true),
                'E' => on_exec(body, st, out, /*with_price*/ false),
                'C' => on_exec(body, st, out, /*with_price*/ true),
                'X' => on_cancel(body, st, out),
                'D' => on_delete(body, st, out),
                'U' => on_replace(body, st, out),
                'P' => on_trade(body, st, out),
                'R' => on_stock_directory(body, st),
                // skip harmlessly
                _ => { /* ignore other admin/metadata messages */ }
            }
        }
    }
}

#[inline]
#[allow(dead_code)] // Used in decode_messages
fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
#[allow(dead_code)]
#[inline]
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
#[allow(dead_code)]
#[inline]
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[inline]
#[allow(dead_code)] // Used by read_u* functions
fn read_fixed<'a, const N: usize>(b: &'a [u8], off: &mut usize) -> Option<&'a [u8; N]> {
    if *off + N <= b.len() {
        // SAFETY: slice length checked
        let ptr = &b[*off..*off + N];
        *off += N;
        Some(ptr.try_into().unwrap())
    } else {
        None
    }
}

#[inline]
#[allow(dead_code)] // Used in message handlers
fn read_u16(b: &[u8], off: &mut usize) -> Option<u16> {
    read_fixed::<2>(b, off).map(|v| u16::from_be_bytes(*v))
}
#[inline]
#[allow(dead_code)] // Used in message handlers
fn read_u32(b: &[u8], off: &mut usize) -> Option<u32> {
    read_fixed::<4>(b, off).map(|v| u32::from_be_bytes(*v))
}
#[inline]
#[allow(dead_code)] // Used in message handlers
fn read_u64(b: &[u8], off: &mut usize) -> Option<u64> {
    read_fixed::<8>(b, off).map(|v| u64::from_be_bytes(*v))
}

#[allow(dead_code)] // Called from decode_messages
fn on_stock_directory(body: &[u8], st: &mut Inner) {
    // 'R' Stock Directory (varies by venue/version). We only keep symbol by locate for debugging.
    // Layout (5.0 typical): locate(2) track(2) ts(6) stock[8] ... (ignore remainder)
    if body.len() < 2 + 2 + 6 + 8 {
        return;
    }
    let mut o = 0usize;
    let locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6; // tracking + timestamp
    if let Some(sym) = read_fixed::<8>(body, &mut o) {
        st.last_symbol_by_locate.insert(locate, *sym);
    }
}

#[allow(dead_code)] // Called from decode_messages
fn on_add(body: &[u8], st: &mut Inner, out: &mut Vec<Event>, with_mpid: bool) {
    // 'A' Add (no MPID) or 'F' Add with MPID (last 4 bytes MPID)
    // Layout:
    // locate(2) track(2) ts(6) order_ref(8) side(1 'B'/'S') shares(4) stock[8] price(4) [mpid(4)?]
    let min_len = 2 + 2 + 6 + 8 + 1 + 4 + 8 + 4 + if with_mpid { 4 } else { 0 };
    if body.len() < min_len {
        return;
    }
    let mut o = 0usize;
    let locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6; // tracking + timestamp
    let order_ref = read_u64(body, &mut o).unwrap();
    let side_ch = body[o];
    o += 1;
    let shares = read_u32(body, &mut o).unwrap() as i64;
    // stock symbol (ignored for book logic)
    let _stock = read_fixed::<8>(body, &mut o).unwrap();
    let price = read_u32(body, &mut o).unwrap() as i64;
    if with_mpid {
        // Ignore MPID bytes; no further fields are read here so no need to advance offset
    }

    let side = if side_ch == b'B' {
        Side::Bid
    } else {
        Side::Ask
    };
    let instr = locate as u32;

    // Emit book event
    out.push(Event::Add {
        order_id: order_ref,
        instr,
        px: price,
        qty: shares,
        side,
    });

    // Track state for subsequent exec/cancel/replace
    st.orders.insert(
        order_ref,
        OrderState {
            instr,
            qty: shares,
            px: price,
            side,
        },
    );
}

#[allow(dead_code)] // Called from decode_messages
fn on_exec(body: &[u8], st: &mut Inner, out: &mut Vec<Event>, _with_price: bool) {
    // 'E' Order Executed (or 'C' Executed w/ Price)
    // Layout:
    // locate(2) track(2) ts(6) order_ref(8) executed_shares(4) match_num(8) [printable(1), exec_price(4)? for 'C']
    if body.len() < 2 + 2 + 6 + 8 + 4 + 8 {
        return;
    }
    let mut o = 0usize;
    let _locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6; // tracking + timestamp
    let order_ref = read_u64(body, &mut o).unwrap();
    let executed = read_u32(body, &mut o).unwrap() as i64;
    // skip match number
    let _ = read_u64(body, &mut o);

    if let Some(s) = st.orders.get_mut(&order_ref).cloned() {
        let new_qty = (s.qty - executed).max(0);
        if new_qty > 0 {
            // emit absolute qty
            out.push(Event::Mod {
                order_id: order_ref,
                qty: new_qty,
            });
            // update state
            if let Some(ent) = st.orders.get_mut(&order_ref) {
                ent.qty = new_qty;
            }
        } else {
            out.push(Event::Del {
                order_id: order_ref,
            });
            st.orders.remove(&order_ref);
        }

        // Emit a trade analytics event (optional, keeps downstream parity)
        out.push(Event::Trade {
            instr: s.instr,
            px: s.px,
            qty: executed,
            maker_order_id: Some(order_ref),
            taker_side: Some(opposite(s.side)),
        });
    } else {
        // If we don't have the order (late join), ignore or route to recovery.
    }
}

#[allow(dead_code)] // Called from decode_messages
fn on_cancel(body: &[u8], st: &mut Inner, out: &mut Vec<Event>) {
    // 'X' Order Cancel (partial reduction)
    // Layout: locate(2) track(2) ts(6) order_ref(8) canceled_shares(4)
    if body.len() < 2 + 2 + 6 + 8 + 4 {
        return;
    }
    let mut o = 0usize;
    let _locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6;
    let order_ref = read_u64(body, &mut o).unwrap();
    let canceled = read_u32(body, &mut o).unwrap() as i64;

    if let Some(ent) = st.orders.get_mut(&order_ref) {
        ent.qty = (ent.qty - canceled).max(0);
        if ent.qty > 0 {
            out.push(Event::Mod {
                order_id: order_ref,
                qty: ent.qty,
            });
        } else {
            out.push(Event::Del {
                order_id: order_ref,
            });
            st.orders.remove(&order_ref);
        }
    }
}

#[allow(dead_code)] // Called from decode_messages
fn on_delete(body: &[u8], st: &mut Inner, out: &mut Vec<Event>) {
    // 'D' Order Delete (remove entire order)
    // Layout: locate(2) track(2) ts(6) order_ref(8)
    if body.len() < 2 + 2 + 6 + 8 {
        return;
    }
    let mut o = 0usize;
    let _locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6;
    let order_ref = read_u64(body, &mut o).unwrap();

    if st.orders.remove(&order_ref).is_some() {
        out.push(Event::Del {
            order_id: order_ref,
        });
    }
}

#[allow(dead_code)] // Called from decode_messages
fn on_replace(body: &[u8], st: &mut Inner, out: &mut Vec<Event>) {
    // 'U' Order Replace
    // Layout: locate(2) track(2) ts(6) orig_ref(8) new_ref(8) shares(4) price(4)
    if body.len() < 2 + 2 + 6 + 8 + 8 + 4 + 4 {
        return;
    }
    let mut o = 0usize;
    let locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6;
    let orig_ref = read_u64(body, &mut o).unwrap();
    let new_ref = read_u64(body, &mut o).unwrap();
    let shares = read_u32(body, &mut o).unwrap() as i64;
    let price = read_u32(body, &mut o).unwrap() as i64;
    let instr = locate as u32;

    // Determine side before removing original entry
    let side = st
        .orders
        .get(&orig_ref)
        .map(|s| s.side)
        .unwrap_or(Side::Bid);
    // Delete original
    if st.orders.remove(&orig_ref).is_some() {
        out.push(Event::Del { order_id: orig_ref });
    }

    // Add new with new id/qty/price, keep side
    out.push(Event::Add {
        order_id: new_ref,
        instr,
        px: price,
        qty: shares,
        side,
    });
    st.orders.insert(
        new_ref,
        OrderState {
            instr,
            qty: shares,
            px: price,
            side,
        },
    );
}

#[allow(dead_code)] // Called from decode_messages
fn on_trade(body: &[u8], st: &mut Inner, out: &mut Vec<Event>) {
    // 'P' Trade (non-cross)
    // Layout: locate(2) track(2) ts(6) order_ref(8) side(1) shares(4) stock[8] price(4) match(8)
    if body.len() < 2 + 2 + 6 + 8 + 1 + 4 + 8 + 4 + 8 {
        return;
    }
    let mut o = 0usize;
    let locate = read_u16(body, &mut o).unwrap();
    o += 2 + 6;
    let order_ref = read_u64(body, &mut o).unwrap();
    let side_ch = body[o];
    o += 1;
    let shares = read_u32(body, &mut o).unwrap() as i64;
    let _stock = read_fixed::<8>(body, &mut o).unwrap();
    let price = read_u32(body, &mut o).unwrap() as i64;
    let _match = read_u64(body, &mut o).unwrap();

    // Reduce maker order if we track it
    if let Some(s) = st.orders.get_mut(&order_ref).cloned() {
        let new_qty = (s.qty - shares).max(0);
        if new_qty > 0 {
            out.push(Event::Mod {
                order_id: order_ref,
                qty: new_qty,
            });
            if let Some(ent) = st.orders.get_mut(&order_ref) {
                ent.qty = new_qty;
                ent.px = price; // some venues send execution price; doesn't change resting price normally
            }
        } else {
            out.push(Event::Del {
                order_id: order_ref,
            });
            st.orders.remove(&order_ref);
        }
        out.push(Event::Trade {
            instr: s.instr,
            px: price,
            qty: shares,
            maker_order_id: Some(order_ref),
            taker_side: Some(opposite(s.side)),
        });
    } else {
        // If we don't know the maker order (e.g., late join), still emit trade analytics
        out.push(Event::Trade {
            instr: locate as u32,
            px: price,
            qty: shares,
            maker_order_id: Some(order_ref),
            taker_side: Some(if side_ch == b'B' {
                Side::Bid
            } else {
                Side::Ask
            }),
        });
    }
}

#[inline]
#[allow(dead_code)] // Used in on_exec and on_trade
fn opposite(s: Side) -> Side {
    match s {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn decode_random_input_does_not_panic(payload in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let dec = Itch50Decoder::new();
            let mut out = Vec::new();
            dec.decode_messages(&payload, &mut out);
            prop_assert!(out.len() <= payload.len());
        }
    }
}
