// Numan - decoder_eobi.rs: EOBI/SBE-like (Eurex/Deutsche Börse style); little-endian SBE header [block_len, template_id, schema, version]; templates; stateless.
//  maps venue messages
// to the engine's Event model. This is not a full Eurex spec, but follows
// SBE framing and common order-flow templates. Hot-path does zero heap allocs.

//
use crate::parser::{Event, MessageDecoder, Side};

#[derive(Default, Clone)]
pub struct EobiSbeDecoder;

impl EobiSbeDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl MessageDecoder for EobiSbeDecoder {
    #[inline]
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>) {
        let mut off = 0usize;
        while off + 8 <= payload.len() {
            let block_len = le_u16(&payload[off..off + 2]) as usize;
            off += 2;
            let template_id = le_u16(&payload[off..off + 2]);
            off += 2;
            let _schema_id = le_u16(&payload[off..off + 2]);
            off += 2;
            let _version = le_u16(&payload[off..off + 2]);
            off += 2;

            if off + block_len > payload.len() {
                break;
            }
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

#[inline]
#[allow(dead_code)] // Used in decode_messages
fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

// Localized unsafe: checked unaligned loads that return None on OOB
#[inline(always)]
fn read_le_u32_checked(b: &[u8], off: usize) -> Option<u32> {
    if off + 4 <= b.len() {
        unsafe {
            let p = b.as_ptr().add(off) as *const u32;
            Some(u32::from_le(p.read_unaligned()))
        }
    } else {
        None
    }
}

#[inline(always)]
fn read_le_u64_checked(b: &[u8], off: usize) -> Option<u64> {
    if off + 8 <= b.len() {
        unsafe {
            let p = b.as_ptr().add(off) as *const u64;
            Some(u64::from_le(p.read_unaligned()))
        }
    } else {
        None
    }
}

#[inline(always)]
fn read_le_i64_checked(b: &[u8], off: usize) -> Option<i64> {
    if off + 8 <= b.len() {
        unsafe {
            let p = b.as_ptr().add(off) as *const i64;
            Some(i64::from_le(p.read_unaligned()))
        }
    } else {
        None
    }
}

#[inline]
#[allow(dead_code)] // Called from decode_messages
fn decode_add(body: &[u8], out: &mut Vec<Event>) {
    const LEN: usize = 8 + 4 + 1 + 8 + 8;
    if body.len() < LEN {
        return;
    }
    // Fixed offsets
    // 0..8: order_id, 8..12: instr, 12: side, 13..21: px, 21..29: qty
    let order_id = match read_le_u64_checked(body, 0) {
        Some(v) => v,
        None => return,
    };
    let instr = match read_le_u32_checked(body, 8) {
        Some(v) => v,
        None => return,
    };
    let side = if body.get(12).copied().unwrap_or(0) == 0 {
        Side::Bid
    } else {
        Side::Ask
    };
    let px = match read_le_i64_checked(body, 13) {
        Some(v) => v,
        None => return,
    };
    let qty = match read_le_i64_checked(body, 21) {
        Some(v) => v,
        None => return,
    };
    out.push(Event::Add {
        order_id,
        instr,
        px,
        qty,
        side,
    });
}

#[inline]
#[allow(dead_code)] // Called from decode_messages
fn decode_mod(body: &[u8], out: &mut Vec<Event>) {
    const LEN: usize = 8 + 8;
    if body.len() < LEN {
        return;
    }
    let order_id = match read_le_u64_checked(body, 0) {
        Some(v) => v,
        None => return,
    };
    let qty = match read_le_i64_checked(body, 8) {
        Some(v) => v,
        None => return,
    };
    out.push(Event::Mod { order_id, qty });
}

#[inline]
#[allow(dead_code)] // Called from decode_messages
fn decode_del(body: &[u8], out: &mut Vec<Event>) {
    if body.len() < 8 {
        return;
    }
    if let Some(order_id) = read_le_u64_checked(body, 0) {
        out.push(Event::Del { order_id });
    }
}

#[inline]
#[allow(dead_code)] // Called from decode_messages
fn decode_trade(body: &[u8], out: &mut Vec<Event>) {
    const LEN: usize = 4 + 8 + 8 + 8 + 1;
    if body.len() < LEN {
        return;
    }
    let instr = match read_le_u32_checked(body, 0) {
        Some(v) => v,
        None => return,
    };
    let px = match read_le_i64_checked(body, 4) {
        Some(v) => v,
        None => return,
    };
    let qty = match read_le_i64_checked(body, 12) {
        Some(v) => v,
        None => return,
    };
    let maker_order_id = match read_le_u64_checked(body, 20) {
        Some(v) => v,
        None => return,
    };
    let b = body.get(28).copied().unwrap_or(2);
    let taker_side = match b {
        0 => Some(Side::Bid),
        1 => Some(Side::Ask),
        _ => None,
    };
    out.push(Event::Trade {
        instr,
        px,
        qty,
        maker_order_id: Some(maker_order_id),
        taker_side,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hdr(block_len: u16, template: u16, schema: u16, version: u16) -> [u8; 8] {
        let mut h = [0u8; 8];
        h[0..2].copy_from_slice(&block_len.to_le_bytes());
        h[2..4].copy_from_slice(&template.to_le_bytes());
        h[4..6].copy_from_slice(&schema.to_le_bytes());
        h[6..8].copy_from_slice(&version.to_le_bytes());
        h
    }

    #[test]
    fn decode_add_ok() {
        let mut buf = Vec::new();
        // body
        let mut body = Vec::new();
        body.extend_from_slice(&123u64.to_le_bytes()); // order_id
        body.extend_from_slice(&(42u32).to_le_bytes()); // instr
        body.push(0u8); // side bid
        body.extend_from_slice(&(1000i64).to_le_bytes()); // px
        body.extend_from_slice(&(10i64).to_le_bytes()); // qty
        let h = hdr(body.len() as u16, 1001, 1, 1);
        buf.extend_from_slice(&h);
        buf.extend_from_slice(&body);

        let dec = EobiSbeDecoder::new();
        let mut out = Vec::new();
        dec.decode_messages(&buf, &mut out);
        match out.as_slice() {
            [Event::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            }] => {
                assert_eq!(*order_id, 123);
                assert_eq!(*instr, 42);
                assert_eq!(*px, 1000);
                assert_eq!(*qty, 10);
                assert!(matches!(side, Side::Bid));
            }
            _ => panic!("unexpected events: {:?}", out),
        }
    }

    #[test]
    fn decode_mod_del_trade_ok() {
        let mut buf = Vec::new();
        // MOD
        let mut b1 = Vec::new();
        b1.extend_from_slice(&123u64.to_le_bytes());
        b1.extend_from_slice(&(5i64).to_le_bytes());
        buf.extend_from_slice(&hdr(b1.len() as u16, 1002, 1, 1));
        buf.extend_from_slice(&b1);
        // DEL
        let mut b2 = Vec::new();
        b2.extend_from_slice(&123u64.to_le_bytes());
        buf.extend_from_slice(&hdr(b2.len() as u16, 1003, 1, 1));
        buf.extend_from_slice(&b2);
        // TRADE
        let mut b3 = Vec::new();
        b3.extend_from_slice(&(7u32).to_le_bytes()); // instr
        b3.extend_from_slice(&(111i64).to_le_bytes()); // px
        b3.extend_from_slice(&(2i64).to_le_bytes()); // qty
        b3.extend_from_slice(&(123u64).to_le_bytes()); // maker
        b3.push(1u8); // taker ask
        buf.extend_from_slice(&hdr(b3.len() as u16, 1004, 1, 1));
        buf.extend_from_slice(&b3);

        let dec = EobiSbeDecoder::new();
        let mut out = Vec::new();
        dec.decode_messages(&buf, &mut out);
        assert!(matches!(
            out[0],
            Event::Mod {
                order_id: 123,
                qty: 5
            }
        ));
        assert!(matches!(out[1], Event::Del { order_id: 123 }));
        match out[2] {
            Event::Trade {
                instr,
                px,
                qty,
                maker_order_id,
                taker_side,
            } => {
                assert_eq!(instr, 7);
                assert_eq!(px, 111);
                assert_eq!(qty, 2);
                assert_eq!(maker_order_id, Some(123));
                assert!(matches!(taker_side, Some(Side::Ask)));
            }
            _ => panic!("expected trade event"),
        }
    }

    proptest! {
        #[test]
        fn decode_random_input_does_not_panic(payload in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let dec = EobiSbeDecoder::new();
            let mut out = Vec::new();
            dec.decode_messages(&payload, &mut out);
            // basic sanity: events len should be bounded by payload size in worst case
            prop_assert!(out.len() <= payload.len());
        }
    }
}
