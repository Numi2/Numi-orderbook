// T7 EOBI decoder backed by generated Deutsche Boerse layout metadata.
//
// The wire message header is the venue MessageHeaderComp:
// BodyLen u16, TemplateID u16, MsgSeqNum u32. BodyLen includes the header.

use crate::decoder_schema::{
    eobi_message, full_order_execution, heartbeat, instrument_state_change, instrument_summary,
    order_add, order_delete, order_mass_delete, order_modify, order_modify_same_prio,
    packet_header, partial_order_execution, product_state_change, product_summary, snapshot_order,
    EventTemplate, EOBI_MESSAGE_HEADER_LEN,
};
use crate::parser::{Event, MessageDecoder, Side, VenueState};
use std::cell::UnsafeCell;

pub struct EobiSbeDecoder {
    inner: UnsafeCell<Inner>,
}

#[derive(Default)]
struct Inner {
    last_msg_seq_num: Option<u32>,
    sequence_gaps: u64,
    last_packet_appl_seq_num: Option<u32>,
    current_snapshot_security_id: Option<i64>,
}

// The parser owns the decoder on the single decode thread. These impls match
// the existing decoder trait bounds without adding synchronization to the hot path.
unsafe impl Send for EobiSbeDecoder {}
unsafe impl Sync for EobiSbeDecoder {}

impl EobiSbeDecoder {
    pub fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner::default()),
        }
    }

    #[cfg(test)]
    fn last_msg_seq_num(&self) -> Option<u32> {
        unsafe { (&*self.inner.get()).last_msg_seq_num }
    }

    #[cfg(test)]
    fn sequence_gaps(&self) -> u64 {
        unsafe { (&*self.inner.get()).sequence_gaps }
    }

    #[cfg(test)]
    fn last_packet_appl_seq_num(&self) -> Option<u32> {
        unsafe { (&*self.inner.get()).last_packet_appl_seq_num }
    }
}

impl Default for EobiSbeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for EobiSbeDecoder {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl MessageDecoder for EobiSbeDecoder {
    #[inline]
    fn decode_messages(&self, payload: &[u8], out: &mut Vec<Event>) {
        let st: &mut Inner = unsafe { &mut *self.inner.get() };
        let mut off = 0usize;

        while off + EOBI_MESSAGE_HEADER_LEN <= payload.len() {
            let body_len = read_le_u16(payload, off) as usize;
            if body_len < EOBI_MESSAGE_HEADER_LEN || off + body_len > payload.len() {
                break;
            }

            let msg = &payload[off..off + body_len];
            off += body_len;

            let template_id = read_le_u16(msg, 2);
            let msg_seq_num = read_le_u32(msg, 4);
            let Some(desc) = eobi_message(template_id) else {
                continue;
            };
            if body_len < desc.min_block_len {
                continue;
            }

            if sequence_bearing(desc.event_template, msg_seq_num) {
                observe_msg_seq(st, msg_seq_num, out);
            }

            match desc.event_template {
                EventTemplate::PacketHeader => decode_packet_header(msg, st),
                EventTemplate::Heartbeat => decode_heartbeat(template_id, msg_seq_num, msg, out),
                EventTemplate::ProductStateChange => {
                    decode_product_state(template_id, msg_seq_num, msg, out)
                }
                EventTemplate::InstrumentStateChange => {
                    decode_instrument_state(template_id, msg_seq_num, msg, out)
                }
                EventTemplate::InstrumentSummary => {
                    decode_instrument_summary(template_id, msg_seq_num, msg, st, out)
                }
                EventTemplate::ProductSummary => {
                    decode_product_summary(template_id, msg_seq_num, msg, out)
                }
                EventTemplate::OrderAdd => decode_order_add(msg, out),
                EventTemplate::OrderModify => decode_order_modify(msg, out),
                EventTemplate::OrderModifySamePrio => decode_order_modify_same_prio(msg, out),
                EventTemplate::OrderDelete => decode_order_delete(msg, out),
                EventTemplate::OrderMassDelete => decode_order_mass_delete(msg, out),
                EventTemplate::PartialOrderExecution => decode_execution(msg, false, out),
                EventTemplate::FullOrderExecution => decode_execution(msg, true, out),
                EventTemplate::SnapshotOrder => decode_snapshot_order(msg, st, out),
                EventTemplate::Unsupported => {}
            }
        }
    }
}

#[inline]
fn sequence_bearing(template: EventTemplate, msg_seq_num: u32) -> bool {
    !matches!(
        template,
        EventTemplate::PacketHeader | EventTemplate::Heartbeat | EventTemplate::Unsupported
    ) && msg_seq_num != u32::MAX
}

#[inline]
fn observe_msg_seq(st: &mut Inner, got: u32, out: &mut Vec<Event>) {
    if let Some(prev) = st.last_msg_seq_num {
        let expected = prev.wrapping_add(1);
        if got != expected {
            st.sequence_gaps = st.sequence_gaps.saturating_add(1);
            out.push(Event::SequenceGap { expected, got });
        }
    }
    st.last_msg_seq_num = Some(got);
}

#[inline]
fn decode_packet_header(msg: &[u8], st: &mut Inner) {
    st.last_packet_appl_seq_num = Some(read_le_u32(msg, packet_header::APPL_SEQ_NUM_OFFSET));
    if read_u8(msg, packet_header::APPL_SEQ_RESET_INDICATOR_OFFSET) == 1 {
        st.last_msg_seq_num = None;
    }
}

#[inline]
fn decode_heartbeat(template_id: u16, msg_seq_num: u32, msg: &[u8], out: &mut Vec<Event>) {
    out.push(Event::State {
        template_id,
        msg_seq_num,
        instr: None,
        state: VenueState::Heartbeat {
            last_msg_seq_num_processed: read_le_u32(
                msg,
                heartbeat::LAST_MSG_SEQ_NUM_PROCESSED_OFFSET,
            ),
        },
    });
    out.push(Event::Heartbeat);
}

#[inline]
fn decode_product_state(template_id: u16, msg_seq_num: u32, msg: &[u8], out: &mut Vec<Event>) {
    out.push(Event::State {
        template_id,
        msg_seq_num,
        instr: None,
        state: VenueState::Product {
            trading_session_id: read_u8(msg, product_state_change::TRADING_SESSION_ID_OFFSET),
            trading_session_sub_id: read_u8(
                msg,
                product_state_change::TRADING_SESSION_SUB_ID_OFFSET,
            ),
            trad_ses_status: read_u8(msg, product_state_change::TRAD_SES_STATUS_OFFSET),
            market_condition: read_u8(msg, product_state_change::MARKET_CONDITION_OFFSET),
            fast_market_indicator: read_u8(msg, product_state_change::FAST_MARKET_INDICATOR_OFFSET),
        },
    });
}

#[inline]
fn decode_instrument_state(template_id: u16, msg_seq_num: u32, msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, instrument_state_change::SECURITY_ID_OFFSET);
    let Some(instr) = instr_from_security_id(security_id) else {
        return;
    };
    out.push(Event::State {
        template_id,
        msg_seq_num,
        instr: Some(instr),
        state: VenueState::Instrument {
            security_status: read_u8(msg, instrument_state_change::SECURITY_STATUS_OFFSET),
            security_trading_status: read_u8(
                msg,
                instrument_state_change::SECURITY_TRADING_STATUS_OFFSET,
            ),
            market_condition: read_u8(msg, instrument_state_change::MARKET_CONDITION_OFFSET),
            fast_market_indicator: read_u8(
                msg,
                instrument_state_change::FAST_MARKET_INDICATOR_OFFSET,
            ),
        },
    });
}

#[inline]
fn decode_instrument_summary(
    template_id: u16,
    msg_seq_num: u32,
    msg: &[u8],
    st: &mut Inner,
    out: &mut Vec<Event>,
) {
    let security_id = read_le_i64(msg, instrument_summary::SECURITY_ID_OFFSET);
    st.current_snapshot_security_id = Some(security_id);
    let Some(instr) = instr_from_security_id(security_id) else {
        return;
    };
    out.push(Event::State {
        template_id,
        msg_seq_num,
        instr: Some(instr),
        state: VenueState::InstrumentSummary {
            tot_no_orders: read_le_u16(msg, instrument_summary::TOT_NO_ORDERS_OFFSET),
            security_status: read_u8(msg, instrument_summary::SECURITY_STATUS_OFFSET),
            security_trading_status: read_u8(
                msg,
                instrument_summary::SECURITY_TRADING_STATUS_OFFSET,
            ),
        },
    });
}

#[inline]
fn decode_product_summary(template_id: u16, msg_seq_num: u32, msg: &[u8], out: &mut Vec<Event>) {
    out.push(Event::State {
        template_id,
        msg_seq_num,
        instr: None,
        state: VenueState::ProductSummary {
            last_msg_seq_num_processed: read_le_u32(
                msg,
                product_summary::LAST_MSG_SEQ_NUM_PROCESSED_OFFSET,
            ),
            trading_session_id: read_u8(msg, product_summary::TRADING_SESSION_ID_OFFSET),
            trading_session_sub_id: read_u8(msg, product_summary::TRADING_SESSION_SUB_ID_OFFSET),
            trad_ses_status: read_u8(msg, product_summary::TRAD_SES_STATUS_OFFSET),
        },
    });
}

#[inline]
fn decode_order_add(msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, order_add::SECURITY_ID_OFFSET);
    let Some(instr) = instr_from_security_id(security_id) else {
        return;
    };
    let Some(side) = read_side(msg, order_add::SIDE_OFFSET) else {
        return;
    };
    let priority = read_le_u64(msg, order_add::TRD_REG_TSTIME_PRIORITY_OFFSET);
    out.push(Event::Add {
        order_id: eobi_order_id(security_id, side, priority),
        instr,
        px: read_decimal(msg, order_add::PRICE_OFFSET),
        qty: read_decimal(msg, order_add::DISPLAY_QTY_OFFSET),
        side,
    });
}

#[inline]
fn decode_order_modify(msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, order_modify::SECURITY_ID_OFFSET);
    let Some(instr) = instr_from_security_id(security_id) else {
        return;
    };
    let Some(side) = read_side(msg, order_modify::SIDE_OFFSET) else {
        return;
    };
    let old_priority = read_le_u64(msg, order_modify::TRD_REG_TSPREV_TIME_PRIORITY_OFFSET);
    let new_priority = read_le_u64(msg, order_modify::TRD_REG_TSTIME_PRIORITY_OFFSET);
    out.push(Event::Del {
        order_id: eobi_order_id(security_id, side, old_priority),
    });
    out.push(Event::Add {
        order_id: eobi_order_id(security_id, side, new_priority),
        instr,
        px: read_decimal(msg, order_modify::PRICE_OFFSET),
        qty: read_decimal(msg, order_modify::DISPLAY_QTY_OFFSET),
        side,
    });
}

#[inline]
fn decode_order_modify_same_prio(msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, order_modify_same_prio::SECURITY_ID_OFFSET);
    let Some(side) = read_side(msg, order_modify_same_prio::SIDE_OFFSET) else {
        return;
    };
    let priority = read_le_u64(msg, order_modify_same_prio::TRD_REG_TSTIME_PRIORITY_OFFSET);
    out.push(Event::Mod {
        order_id: eobi_order_id(security_id, side, priority),
        qty: read_decimal(msg, order_modify_same_prio::DISPLAY_QTY_OFFSET),
    });
}

#[inline]
fn decode_order_delete(msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, order_delete::SECURITY_ID_OFFSET);
    let Some(side) = read_side(msg, order_delete::SIDE_OFFSET) else {
        return;
    };
    let priority = read_le_u64(msg, order_delete::TRD_REG_TSTIME_PRIORITY_OFFSET);
    out.push(Event::Del {
        order_id: eobi_order_id(security_id, side, priority),
    });
}

#[inline]
fn decode_order_mass_delete(msg: &[u8], out: &mut Vec<Event>) {
    let security_id = read_le_i64(msg, order_mass_delete::SECURITY_ID_OFFSET);
    if let Some(instr) = instr_from_security_id(security_id) {
        out.push(Event::MassDel { instr });
    }
}

#[inline]
fn decode_execution(msg: &[u8], full: bool, out: &mut Vec<Event>) {
    let (security_id, side, priority, instr, qty, px, match_id) = if full {
        execution_parts::<true>(msg)
    } else {
        execution_parts::<false>(msg)
    };
    let Some(side) = side else {
        return;
    };
    let Some(instr) = instr else {
        return;
    };
    out.push(Event::Execute {
        instr,
        px,
        qty,
        order_id: eobi_order_id(security_id, side, priority),
        taker_side: None,
        match_id,
        full,
    });
}

#[inline]
fn execution_parts<const FULL: bool>(
    msg: &[u8],
) -> (i64, Option<Side>, u64, Option<u32>, i64, i64, u32) {
    if FULL {
        let security_id = read_le_i64(msg, full_order_execution::SECURITY_ID_OFFSET);
        (
            security_id,
            read_side(msg, full_order_execution::SIDE_OFFSET),
            read_le_u64(msg, full_order_execution::TRD_REG_TSTIME_PRIORITY_OFFSET),
            instr_from_security_id(security_id),
            read_decimal(msg, full_order_execution::LAST_QTY_OFFSET),
            read_decimal(msg, full_order_execution::LAST_PX_OFFSET),
            read_le_u32(msg, full_order_execution::TRD_MATCH_ID_OFFSET),
        )
    } else {
        let security_id = read_le_i64(msg, partial_order_execution::SECURITY_ID_OFFSET);
        (
            security_id,
            read_side(msg, partial_order_execution::SIDE_OFFSET),
            read_le_u64(msg, partial_order_execution::TRD_REG_TSTIME_PRIORITY_OFFSET),
            instr_from_security_id(security_id),
            read_decimal(msg, partial_order_execution::LAST_QTY_OFFSET),
            read_decimal(msg, partial_order_execution::LAST_PX_OFFSET),
            read_le_u32(msg, partial_order_execution::TRD_MATCH_ID_OFFSET),
        )
    }
}

#[inline]
fn decode_snapshot_order(msg: &[u8], st: &Inner, out: &mut Vec<Event>) {
    let Some(security_id) = st.current_snapshot_security_id else {
        return;
    };
    let Some(instr) = instr_from_security_id(security_id) else {
        return;
    };
    let Some(side) = read_side(msg, snapshot_order::SIDE_OFFSET) else {
        return;
    };
    let priority = read_le_u64(msg, snapshot_order::TRD_REG_TSTIME_PRIORITY_OFFSET);
    out.push(Event::Add {
        order_id: eobi_order_id(security_id, side, priority),
        instr,
        px: read_decimal(msg, snapshot_order::PRICE_OFFSET),
        qty: read_decimal(msg, snapshot_order::DISPLAY_QTY_OFFSET),
        side,
    });
}

#[inline]
fn instr_from_security_id(security_id: i64) -> Option<u32> {
    u32::try_from(security_id).ok()
}

#[inline]
fn read_side(msg: &[u8], off: usize) -> Option<Side> {
    match read_u8(msg, off) {
        1 => Some(Side::Bid),
        2 => Some(Side::Ask),
        _ => None,
    }
}

#[inline]
fn read_decimal(msg: &[u8], off: usize) -> i64 {
    read_le_i64(msg, off)
}

#[inline]
fn eobi_order_id(security_id: i64, side: Side, priority: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in security_id.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= match side {
        Side::Bid => 1,
        Side::Ask => 2,
    };
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    for b in priority.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if h == 0 {
        1
    } else {
        h
    }
}

#[inline]
fn read_u8(b: &[u8], off: usize) -> u8 {
    b[off]
}

#[inline]
fn read_le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn read_le_u32(b: &[u8], off: usize) -> u32 {
    unsafe { u32::from_le((b.as_ptr().add(off) as *const u32).read_unaligned()) }
}

#[inline]
fn read_le_u64(b: &[u8], off: usize) -> u64 {
    unsafe { u64::from_le((b.as_ptr().add(off) as *const u64).read_unaligned()) }
}

#[inline]
fn read_le_i64(b: &[u8], off: usize) -> i64 {
    unsafe { i64::from_le((b.as_ptr().add(off) as *const i64).read_unaligned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orderbook::{OrderBook, OrderBookCapacity};
    use proptest::prelude::*;

    fn msg(template_id: u16, msg_seq_num: u32, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        b[0..2].copy_from_slice(&(len as u16).to_le_bytes());
        b[2..4].copy_from_slice(&template_id.to_le_bytes());
        b[4..8].copy_from_slice(&msg_seq_num.to_le_bytes());
        b
    }

    fn put_u8(buf: &mut [u8], off: usize, value: u8) {
        buf[off] = value;
    }

    fn put_u16(buf: &mut [u8], off: usize, value: u16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(buf: &mut [u8], off: usize, value: u32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i32(buf: &mut [u8], off: usize, value: i32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(buf: &mut [u8], off: usize, value: u64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i64(buf: &mut [u8], off: usize, value: i64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn packet_header(appl_seq_num: u32) -> Vec<u8> {
        let mut b = msg(
            packet_header::TEMPLATE_ID,
            u32::MAX,
            packet_header::MIN_BLOCK_LEN,
        );
        put_u32(&mut b, packet_header::APPL_SEQ_NUM_OFFSET, appl_seq_num);
        put_i32(&mut b, packet_header::MARKET_SEGMENT_ID_OFFSET, 77);
        put_u8(&mut b, packet_header::PARTITION_ID_OFFSET, 3);
        put_u8(&mut b, packet_header::COMPLETION_INDICATOR_OFFSET, 1);
        b
    }

    fn order_add(
        seq: u32,
        security_id: i64,
        side: u8,
        priority: u64,
        px: i64,
        qty: i64,
    ) -> Vec<u8> {
        let mut b = msg(order_add::TEMPLATE_ID, seq, order_add::MIN_BLOCK_LEN);
        put_i64(&mut b, order_add::SECURITY_ID_OFFSET, security_id);
        put_u64(&mut b, order_add::TRD_REG_TSTIME_PRIORITY_OFFSET, priority);
        put_i64(&mut b, order_add::DISPLAY_QTY_OFFSET, qty);
        put_u8(&mut b, order_add::SIDE_OFFSET, side);
        put_i64(&mut b, order_add::PRICE_OFFSET, px);
        b
    }

    fn same_priority_modify(
        seq: u32,
        security_id: i64,
        side: u8,
        priority: u64,
        qty: i64,
    ) -> Vec<u8> {
        let mut b = msg(
            order_modify_same_prio::TEMPLATE_ID,
            seq,
            order_modify_same_prio::MIN_BLOCK_LEN,
        );
        put_i64(
            &mut b,
            order_modify_same_prio::SECURITY_ID_OFFSET,
            security_id,
        );
        put_u64(
            &mut b,
            order_modify_same_prio::TRD_REG_TSTIME_PRIORITY_OFFSET,
            priority,
        );
        put_i64(&mut b, order_modify_same_prio::DISPLAY_QTY_OFFSET, qty);
        put_u8(&mut b, order_modify_same_prio::SIDE_OFFSET, side);
        b
    }

    struct ExecutionFixture {
        template_id: u16,
        seq: u32,
        security_id: i64,
        side: u8,
        priority: u64,
        px: i64,
        qty: i64,
        match_id: u32,
    }

    fn execution(fixture: ExecutionFixture) -> Vec<u8> {
        let mut b = msg(
            fixture.template_id,
            fixture.seq,
            partial_order_execution::MIN_BLOCK_LEN,
        );
        put_u8(&mut b, partial_order_execution::SIDE_OFFSET, fixture.side);
        put_u32(
            &mut b,
            partial_order_execution::TRD_MATCH_ID_OFFSET,
            fixture.match_id,
        );
        put_i64(
            &mut b,
            partial_order_execution::SECURITY_ID_OFFSET,
            fixture.security_id,
        );
        put_u64(
            &mut b,
            partial_order_execution::TRD_REG_TSTIME_PRIORITY_OFFSET,
            fixture.priority,
        );
        put_i64(
            &mut b,
            partial_order_execution::LAST_QTY_OFFSET,
            fixture.qty,
        );
        put_i64(&mut b, partial_order_execution::LAST_PX_OFFSET, fixture.px);
        b
    }

    fn mass_delete(seq: u32, security_id: i64) -> Vec<u8> {
        let mut b = msg(
            order_mass_delete::TEMPLATE_ID,
            seq,
            order_mass_delete::MIN_BLOCK_LEN,
        );
        put_i64(&mut b, order_mass_delete::SECURITY_ID_OFFSET, security_id);
        b
    }

    fn instrument_summary(seq: u32, security_id: i64, orders: u16) -> Vec<u8> {
        let mut b = msg(
            instrument_summary::TEMPLATE_ID,
            seq,
            instrument_summary::MIN_BLOCK_LEN,
        );
        put_i64(&mut b, instrument_summary::SECURITY_ID_OFFSET, security_id);
        put_u16(&mut b, instrument_summary::TOT_NO_ORDERS_OFFSET, orders);
        put_u8(&mut b, instrument_summary::SECURITY_STATUS_OFFSET, 1);
        put_u8(
            &mut b,
            instrument_summary::SECURITY_TRADING_STATUS_OFFSET,
            203,
        );
        b
    }

    fn snapshot_order(seq: u32, side: u8, priority: u64, px: i64, qty: i64) -> Vec<u8> {
        let mut b = msg(
            snapshot_order::TEMPLATE_ID,
            seq,
            snapshot_order::MIN_BLOCK_LEN,
        );
        put_u64(
            &mut b,
            snapshot_order::TRD_REG_TSTIME_PRIORITY_OFFSET,
            priority,
        );
        put_i64(&mut b, snapshot_order::DISPLAY_QTY_OFFSET, qty);
        put_u8(&mut b, snapshot_order::SIDE_OFFSET, side);
        put_i64(&mut b, snapshot_order::PRICE_OFFSET, px);
        b
    }

    fn product_state(seq: u32) -> Vec<u8> {
        let mut b = msg(
            product_state_change::TEMPLATE_ID,
            seq,
            product_state_change::MIN_BLOCK_LEN,
        );
        put_u8(&mut b, product_state_change::TRADING_SESSION_ID_OFFSET, 1);
        put_u8(
            &mut b,
            product_state_change::TRADING_SESSION_SUB_ID_OFFSET,
            3,
        );
        put_u8(&mut b, product_state_change::TRAD_SES_STATUS_OFFSET, 2);
        put_u8(&mut b, product_state_change::MARKET_CONDITION_OFFSET, 0);
        put_u8(
            &mut b,
            product_state_change::FAST_MARKET_INDICATOR_OFFSET,
            0,
        );
        b
    }

    fn instrument_state(seq: u32, security_id: i64) -> Vec<u8> {
        let mut b = msg(
            instrument_state_change::TEMPLATE_ID,
            seq,
            instrument_state_change::MIN_BLOCK_LEN,
        );
        put_i64(
            &mut b,
            instrument_state_change::SECURITY_ID_OFFSET,
            security_id,
        );
        put_u8(&mut b, instrument_state_change::SECURITY_STATUS_OFFSET, 1);
        put_u8(
            &mut b,
            instrument_state_change::SECURITY_TRADING_STATUS_OFFSET,
            203,
        );
        put_u8(&mut b, instrument_state_change::MARKET_CONDITION_OFFSET, 0);
        put_u8(
            &mut b,
            instrument_state_change::FAST_MARKET_INDICATOR_OFFSET,
            0,
        );
        b
    }

    fn decode_fixture(messages: Vec<Vec<u8>>) -> (EobiSbeDecoder, Vec<Event>) {
        let mut payload = Vec::new();
        for message in messages {
            payload.extend_from_slice(&message);
        }
        let dec = EobiSbeDecoder::new();
        let mut out = Vec::new();
        dec.decode_messages(&payload, &mut out);
        (dec, out)
    }

    fn book_with_capacity() -> OrderBook {
        OrderBook::new_with_tick_table_and_capacity(
            10,
            false,
            64,
            1,
            128,
            [(42, 1)],
            OrderBookCapacity::default(),
        )
        .unwrap()
    }

    #[test]
    fn replay_fixture_same_priority_modify_updates_quantity() {
        let (_dec, out) = decode_fixture(vec![
            order_add(1, 42, 1, 1_000, 100_000_000, 10_000),
            same_priority_modify(2, 42, 1, 1_000, 7_000),
        ]);
        assert!(matches!(out[0], Event::Add { qty: 10_000, .. }));
        assert!(matches!(out[1], Event::Mod { qty: 7_000, .. }));

        let mut book = book_with_capacity();
        for ev in &out {
            book.apply(ev);
        }
        assert_eq!(book.order_count(), 1);
        assert_eq!(book.bbo().0, Some((100_000_000, 7_000)));
    }

    #[test]
    fn replay_fixture_partial_and_full_execution_update_book() {
        let (_dec, out) = decode_fixture(vec![
            order_add(1, 42, 1, 1_000, 100_000_000, 10_000),
            execution(ExecutionFixture {
                template_id: partial_order_execution::TEMPLATE_ID,
                seq: 2,
                security_id: 42,
                side: 1,
                priority: 1_000,
                px: 100_000_000,
                qty: 4_000,
                match_id: 900,
            }),
            execution(ExecutionFixture {
                template_id: full_order_execution::TEMPLATE_ID,
                seq: 3,
                security_id: 42,
                side: 1,
                priority: 1_000,
                px: 100_000_000,
                qty: 6_000,
                match_id: 901,
            }),
        ]);
        assert!(matches!(
            out[1],
            Event::Execute {
                qty: 4_000,
                full: false,
                ..
            }
        ));
        assert!(matches!(
            out[2],
            Event::Execute {
                qty: 6_000,
                full: true,
                ..
            }
        ));

        let mut book = book_with_capacity();
        for ev in &out {
            book.apply(ev);
        }
        assert_eq!(book.order_count(), 0);
    }

    #[test]
    fn replay_fixture_mass_delete_clears_instrument() {
        let (_dec, out) = decode_fixture(vec![
            order_add(1, 42, 1, 1_000, 100_000_000, 10_000),
            order_add(2, 42, 2, 1_010, 100_000_100, 5_000),
            mass_delete(3, 42),
        ]);
        assert!(matches!(out[2], Event::MassDel { instr: 42 }));

        let mut book = book_with_capacity();
        for ev in &out {
            book.apply(ev);
        }
        assert_eq!(book.order_count(), 0);
    }

    #[test]
    fn replay_fixture_snapshot_order_uses_instrument_summary_context() {
        let (_dec, out) = decode_fixture(vec![
            instrument_summary(1, 42, 1),
            snapshot_order(2, 2, 2_000, 100_000_200, 11_000),
        ]);
        assert!(matches!(
            out[0],
            Event::State {
                instr: Some(42),
                state: VenueState::InstrumentSummary {
                    tot_no_orders: 1,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            out[1],
            Event::Add {
                instr: 42,
                side: Side::Ask,
                px: 100_000_200,
                qty: 11_000,
                ..
            }
        ));
    }

    #[test]
    fn replay_fixture_state_messages_are_decoded() {
        let (_dec, out) = decode_fixture(vec![product_state(1), instrument_state(2, 42)]);
        assert!(matches!(
            out[0],
            Event::State {
                template_id: 13300,
                msg_seq_num: 1,
                state: VenueState::Product {
                    trad_ses_status: 2,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            out[1],
            Event::State {
                template_id: 13301,
                msg_seq_num: 2,
                instr: Some(42),
                state: VenueState::Instrument {
                    security_trading_status: 203,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn replay_fixture_message_sequencing_tracks_packet_and_gaps() {
        let (dec, out) = decode_fixture(vec![
            packet_header(10),
            order_add(1, 42, 1, 1_000, 100_000_000, 10_000),
            same_priority_modify(3, 42, 1, 1_000, 9_000),
        ]);
        assert_eq!(dec.last_packet_appl_seq_num(), Some(10));
        assert_eq!(dec.last_msg_seq_num(), Some(3));
        assert_eq!(dec.sequence_gaps(), 1);
        assert!(matches!(
            out[1],
            Event::SequenceGap {
                expected: 2,
                got: 3
            }
        ));
    }

    #[test]
    fn generated_body_len_and_template_are_required() {
        let mut bad = order_add(1, 42, 1, 1_000, 100, 10);
        bad[0..2].copy_from_slice(&(7u16).to_le_bytes());
        let (_dec, out) = decode_fixture(vec![bad]);
        assert!(out.is_empty());
    }

    proptest! {
        #[test]
        fn decode_random_input_does_not_panic(payload in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let dec = EobiSbeDecoder::new();
            let mut out = Vec::new();
            dec.decode_messages(&payload, &mut out);
            prop_assert!(out.len() <= payload.len());
        }
    }
}
