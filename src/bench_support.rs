use crate::codec_raw::{OboAddV1, OboCancelV1, OboExecuteV1, OboModifyV1};
use crate::decoder_schema::{
    full_order_execution, order_add, order_delete, order_modify_same_prio, packet_header,
    partial_order_execution,
};
use crate::orderbook::{OrderBook, OrderBookCapacity};
use crate::parser::{Event, Side};
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
use zerocopy::AsBytes;

#[derive(Debug, Clone, Copy)]
pub struct FixtureConfig {
    pub instruments: usize,
    pub orders_per_instrument: usize,
    pub packet_count: usize,
    pub messages_per_packet: usize,
    pub seed: u64,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self {
            instruments: 64,
            orders_per_instrument: 64,
            packet_count: 4096,
            messages_per_packet: 4,
            seed: 0x9e37_79b9_7f4a_7c15,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkFixtures {
    pub events: Vec<Event>,
    pub eobi_packets: Vec<Vec<u8>>,
    pub itch_payload: Vec<u8>,
    pub fast_payload: Vec<u8>,
    pub raw_obo_payloads: Vec<Vec<u8>>,
    pub expected_state_hash: u64,
}

impl BenchmarkFixtures {
    pub fn new(cfg: FixtureConfig) -> Self {
        let events = mixed_l3_events(cfg);
        let expected_state_hash = state_hash_after_events(cfg, &events);
        Self {
            events,
            eobi_packets: eobi_packets(cfg),
            itch_payload: itch_payload(cfg),
            fast_payload: fast_payload(cfg),
            raw_obo_payloads: raw_obo_payloads(),
            expected_state_hash,
        }
    }
}

pub fn benchmark_order_book(cfg: FixtureConfig) -> OrderBook {
    let total_orders = cfg.instruments.saturating_mul(cfg.orders_per_instrument);
    let ticks = (1..=cfg.instruments as u32).map(|instr| (instr, 1));
    OrderBook::new_with_tick_table_and_capacity(
        50,
        false,
        cfg.orders_per_instrument.max(1),
        1,
        16_384,
        ticks,
        OrderBookCapacity {
            instruments: cfg.instruments,
            global_order_index: total_orders,
            per_instrument_order_index: cfg.orders_per_instrument.max(1),
            preallocate_instrument_books: true,
        },
    )
    .expect("benchmark fixture tick table is valid")
}

pub fn mixed_l3_events(cfg: FixtureConfig) -> Vec<Event> {
    let total_orders = cfg.instruments.saturating_mul(cfg.orders_per_instrument);
    let mut events = Vec::with_capacity(total_orders + total_orders / 2);

    for idx in 0..total_orders {
        let spec = order_spec(cfg, idx);
        events.push(Event::Add {
            order_id: spec.order_id,
            instr: spec.instr,
            px: spec.price,
            qty: spec.qty,
            side: spec.side,
        });
    }

    for idx in (0..total_orders).step_by(4) {
        let spec = order_spec(cfg, idx);
        events.push(Event::Mod {
            order_id: spec.order_id,
            qty: spec.qty + 17,
        });
    }

    for idx in (1..total_orders).step_by(5) {
        let spec = order_spec(cfg, idx);
        events.push(Event::Execute {
            instr: spec.instr,
            px: spec.price,
            qty: (spec.qty / 4).max(1),
            order_id: spec.order_id,
            taker_side: Some(opposite(spec.side)),
            match_id: idx as u32,
            full: false,
        });
    }

    for idx in (2..total_orders).step_by(7) {
        let spec = order_spec(cfg, idx);
        events.push(Event::Del {
            order_id: spec.order_id,
        });
    }

    events
}

pub fn state_hash_after_events(cfg: FixtureConfig, events: &[Event]) -> u64 {
    let mut book = benchmark_order_book(cfg);
    for event in events {
        book.apply(event);
    }
    book.state_hash()
}

pub fn read_capture_payloads(path: &Path, limit: usize) -> std::io::Result<Vec<Vec<u8>>> {
    let data = fs::read(path)?;
    if data.len() < 24 {
        return Ok(Vec::new());
    }

    let endian = match &data[0..4] {
        [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => Endian::Le,
        [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => Endian::Be,
        _ => return Ok(vec![data]),
    };

    let mut payloads = Vec::new();
    let mut off = 24usize;
    while off + 16 <= data.len() && (limit == 0 || payloads.len() < limit) {
        let incl_len = read_u32(&data, off + 8, endian) as usize;
        off += 16;
        if off + incl_len > data.len() {
            break;
        }
        let packet = &data[off..off + incl_len];
        off += incl_len;
        payloads.push(extract_udp_payload(packet).unwrap_or(packet).to_vec());
    }
    Ok(payloads)
}

pub fn stable_config_hash(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn format_kv_line<I, K, V>(fields: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    fields
        .into_iter()
        .map(|(key, value)| format!("{}={}", key.as_ref(), escape_value(value.as_ref())))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_kv_line(line: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for part in line.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("missing '=' in field {part:?}"));
        };
        if key.is_empty() {
            return Err("empty key".to_string());
        }
        out.insert(key.to_string(), unescape_value(value)?);
    }
    Ok(out)
}

fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'%' => out.push_str("%25"),
            b' ' => out.push_str("%20"),
            b'\t' => out.push_str("%09"),
            b'\n' => out.push_str("%0A"),
            b'\r' => out.push_str("%0D"),
            b'=' => out.push_str("%3D"),
            _ => out.push(byte as char),
        }
    }
    out
}

fn unescape_value(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'%' {
            out.push(bytes[idx]);
            idx += 1;
            continue;
        }
        if idx + 2 >= bytes.len() {
            return Err(format!("truncated percent escape in {value:?}"));
        }
        let hi = hex_value(bytes[idx + 1])?;
        let lo = hex_value(bytes[idx + 2])?;
        out.push((hi << 4) | lo);
        idx += 3;
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid hex byte {:?}", byte as char)),
    }
}

#[derive(Debug, Clone, Copy)]
struct OrderSpec {
    order_id: u64,
    instr: u32,
    security_id: i64,
    side: Side,
    eobi_side: u8,
    priority: u64,
    price: i64,
    qty: i64,
}

fn order_spec(cfg: FixtureConfig, idx: usize) -> OrderSpec {
    let instruments = cfg.instruments.max(1);
    let instr = (idx % instruments) as u32 + 1;
    let local = idx / instruments;
    let side = if (idx.wrapping_add(local) & 1) == 0 {
        Side::Bid
    } else {
        Side::Ask
    };
    let eobi_side = match side {
        Side::Bid => 1,
        Side::Ask => 2,
    };
    let mid = 1_000_000_i64 + (instr as i64 * 100);
    let price_offset = ((local % 101) as i64) - 50;
    let price = match side {
        Side::Bid => mid - price_offset.abs(),
        Side::Ask => mid + price_offset.abs(),
    };
    let qty = 100 + ((idx.wrapping_mul(17) ^ cfg.seed as usize) % 900) as i64;
    let order_id = ((instr as u64) << 40) | (local as u64 + 1);
    OrderSpec {
        order_id,
        instr,
        security_id: i64::from(instr),
        side,
        eobi_side,
        priority: 10_000_000 + idx as u64,
        price,
        qty,
    }
}

fn eobi_packets(cfg: FixtureConfig) -> Vec<Vec<u8>> {
    let mut packets = Vec::with_capacity(cfg.packet_count);
    let mut msg_seq = 1u32;
    let total_orders = cfg.instruments.max(1) * cfg.orders_per_instrument.max(1);

    for packet_idx in 0..cfg.packet_count {
        let mut payload = eobi_packet_header(packet_idx as u32 + 1);
        for _ in 0..cfg.messages_per_packet.max(1) {
            let event_idx = (msg_seq as usize - 1) % (total_orders + total_orders / 2).max(1);
            let order_idx = event_idx % total_orders;
            let spec = order_spec(cfg, order_idx);
            let message = if event_idx < total_orders {
                eobi_order_add(msg_seq, spec)
            } else {
                match event_idx % 3 {
                    0 => eobi_same_priority_modify(msg_seq, spec),
                    1 => eobi_partial_execution(msg_seq, spec, false),
                    _ => eobi_order_delete(msg_seq, spec),
                }
            };
            payload.extend_from_slice(&message);
            msg_seq = msg_seq.wrapping_add(1);
        }
        packets.push(payload);
    }

    packets
}

fn eobi_message(template_id: u16, msg_seq_num: u32, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    put_u16_le(&mut out, 0, len as u16);
    put_u16_le(&mut out, 2, template_id);
    put_u32_le(&mut out, 4, msg_seq_num);
    out
}

fn eobi_packet_header(appl_seq_num: u32) -> Vec<u8> {
    let mut out = eobi_message(
        packet_header::TEMPLATE_ID,
        u32::MAX,
        packet_header::MIN_BLOCK_LEN,
    );
    put_u32_le(&mut out, packet_header::APPL_SEQ_NUM_OFFSET, appl_seq_num);
    put_i32_le(&mut out, packet_header::MARKET_SEGMENT_ID_OFFSET, 77);
    out[packet_header::PARTITION_ID_OFFSET] = 3;
    out[packet_header::COMPLETION_INDICATOR_OFFSET] = 1;
    out
}

fn eobi_order_add(seq: u32, spec: OrderSpec) -> Vec<u8> {
    let mut out = eobi_message(order_add::TEMPLATE_ID, seq, order_add::MIN_BLOCK_LEN);
    put_i64_le(&mut out, order_add::SECURITY_ID_OFFSET, spec.security_id);
    put_u64_le(
        &mut out,
        order_add::TRD_REG_TSTIME_PRIORITY_OFFSET,
        spec.priority,
    );
    put_i64_le(&mut out, order_add::DISPLAY_QTY_OFFSET, spec.qty);
    out[order_add::SIDE_OFFSET] = spec.eobi_side;
    put_i64_le(&mut out, order_add::PRICE_OFFSET, spec.price);
    out
}

fn eobi_same_priority_modify(seq: u32, spec: OrderSpec) -> Vec<u8> {
    let mut out = eobi_message(
        order_modify_same_prio::TEMPLATE_ID,
        seq,
        order_modify_same_prio::MIN_BLOCK_LEN,
    );
    put_i64_le(
        &mut out,
        order_modify_same_prio::SECURITY_ID_OFFSET,
        spec.security_id,
    );
    put_u64_le(
        &mut out,
        order_modify_same_prio::TRD_REG_TSTIME_PRIORITY_OFFSET,
        spec.priority,
    );
    put_i64_le(
        &mut out,
        order_modify_same_prio::DISPLAY_QTY_OFFSET,
        spec.qty + 11,
    );
    out[order_modify_same_prio::SIDE_OFFSET] = spec.eobi_side;
    put_i64_le(&mut out, order_modify_same_prio::PRICE_OFFSET, spec.price);
    out
}

fn eobi_order_delete(seq: u32, spec: OrderSpec) -> Vec<u8> {
    let mut out = eobi_message(order_delete::TEMPLATE_ID, seq, order_delete::MIN_BLOCK_LEN);
    put_i64_le(&mut out, order_delete::SECURITY_ID_OFFSET, spec.security_id);
    put_u64_le(
        &mut out,
        order_delete::TRD_REG_TSTIME_PRIORITY_OFFSET,
        spec.priority,
    );
    put_i64_le(&mut out, order_delete::DISPLAY_QTY_OFFSET, spec.qty);
    out[order_delete::SIDE_OFFSET] = spec.eobi_side;
    put_i64_le(&mut out, order_delete::PRICE_OFFSET, spec.price);
    out
}

fn eobi_partial_execution(seq: u32, spec: OrderSpec, full: bool) -> Vec<u8> {
    let template = if full {
        full_order_execution::TEMPLATE_ID
    } else {
        partial_order_execution::TEMPLATE_ID
    };
    let len = if full {
        full_order_execution::MIN_BLOCK_LEN
    } else {
        partial_order_execution::MIN_BLOCK_LEN
    };
    let mut out = eobi_message(template, seq, len);
    out[partial_order_execution::SIDE_OFFSET] = spec.eobi_side;
    put_u32_le(&mut out, partial_order_execution::TRD_MATCH_ID_OFFSET, seq);
    put_i64_le(
        &mut out,
        partial_order_execution::SECURITY_ID_OFFSET,
        spec.security_id,
    );
    put_u64_le(
        &mut out,
        partial_order_execution::TRD_REG_TSTIME_PRIORITY_OFFSET,
        spec.priority,
    );
    put_i64_le(
        &mut out,
        partial_order_execution::LAST_QTY_OFFSET,
        (spec.qty / 4).max(1),
    );
    put_i64_le(
        &mut out,
        partial_order_execution::LAST_PX_OFFSET,
        spec.price,
    );
    out
}

fn itch_payload(cfg: FixtureConfig) -> Vec<u8> {
    let mut payload = Vec::new();
    let total = cfg
        .instruments
        .saturating_mul(cfg.orders_per_instrument)
        .clamp(1, 1024);
    for idx in 0..total {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::with_capacity(35);
        put_u16_be_vec(&mut body, spec.instr as u16);
        put_u16_be_vec(&mut body, 0);
        body.extend_from_slice(&[0; 6]);
        put_u64_be_vec(&mut body, spec.order_id);
        body.push(match spec.side {
            Side::Bid => b'B',
            Side::Ask => b'S',
        });
        put_u32_be_vec(&mut body, spec.qty as u32);
        body.extend_from_slice(b"NUMI    ");
        put_u32_be_vec(&mut body, spec.price as u32);
        push_itch_message(&mut payload, b'A', &body);
    }
    for idx in (0..total).step_by(4) {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::with_capacity(22);
        put_u16_be_vec(&mut body, spec.instr as u16);
        put_u16_be_vec(&mut body, 0);
        body.extend_from_slice(&[0; 6]);
        put_u64_be_vec(&mut body, spec.order_id);
        put_u32_be_vec(&mut body, (spec.qty / 3).max(1) as u32);
        push_itch_message(&mut payload, b'X', &body);
    }
    for idx in (1..total).step_by(7) {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::with_capacity(18);
        put_u16_be_vec(&mut body, spec.instr as u16);
        put_u16_be_vec(&mut body, 0);
        body.extend_from_slice(&[0; 6]);
        put_u64_be_vec(&mut body, spec.order_id);
        push_itch_message(&mut payload, b'D', &body);
    }
    payload
}

fn fast_payload(cfg: FixtureConfig) -> Vec<u8> {
    let mut payload = Vec::new();
    let total = cfg
        .instruments
        .saturating_mul(cfg.orders_per_instrument)
        .clamp(1, 1024);
    for idx in 0..total {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::new();
        write_varint(spec.order_id, &mut body);
        write_varint(u64::from(spec.instr), &mut body);
        body.push(match spec.side {
            Side::Bid => 0,
            Side::Ask => 1,
        });
        write_varint(zigzag(spec.price), &mut body);
        write_varint(zigzag(spec.qty), &mut body);
        write_fast_message(0, 1, &body, &mut payload);
    }
    for idx in (0..total).step_by(4) {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::new();
        write_varint(spec.order_id, &mut body);
        write_varint(zigzag(spec.qty + 13), &mut body);
        write_fast_message(0, 2, &body, &mut payload);
    }
    for idx in (1..total).step_by(8) {
        let spec = order_spec(cfg, idx);
        let mut body = Vec::new();
        write_varint(spec.order_id, &mut body);
        write_fast_message(0, 3, &body, &mut payload);
    }
    payload
}

fn raw_obo_payloads() -> Vec<Vec<u8>> {
    vec![
        OboAddV1 {
            order_id: 1,
            price_e8: 1_000_000,
            qty: 100,
            side: 0,
            flags: 0,
        }
        .as_bytes()
        .to_vec(),
        OboModifyV1 {
            order_id: 1,
            new_price_e8: 0,
            new_qty: 120,
            flags: 1,
        }
        .as_bytes()
        .to_vec(),
        OboExecuteV1 {
            maker_order_id: 1,
            trade_qty: 25,
            trade_price_e8: 1_000_000,
            aggressor_side: 1,
            match_id: 9,
        }
        .as_bytes()
        .to_vec(),
        OboCancelV1 {
            order_id: 1,
            qty_cxl: 0,
            reason: 0,
        }
        .as_bytes()
        .to_vec(),
    ]
}

fn push_itch_message(payload: &mut Vec<u8>, typ: u8, body: &[u8]) {
    let len = body.len() + 1;
    put_u16_be_vec(payload, len as u16);
    payload.push(typ);
    payload.extend_from_slice(body);
}

fn write_fast_message(pmap: u64, template_id: u64, body: &[u8], out: &mut Vec<u8>) {
    write_varint(pmap, out);
    write_varint(template_id, out);
    write_varint(body.len() as u64, out);
    out.extend_from_slice(body);
}

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Le,
    Be,
}

fn read_u32(bytes: &[u8], off: usize, endian: Endian) -> u32 {
    let raw = [bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]];
    match endian {
        Endian::Le => u32::from_le_bytes(raw),
        Endian::Be => u32::from_be_bytes(raw),
    }
}

fn extract_udp_payload(frame: &[u8]) -> Option<&[u8]> {
    if frame.len() < 14 {
        return None;
    }
    let mut l3 = 14usize;
    let mut ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype == 0x8100 || ethertype == 0x88a8 {
        if frame.len() < 18 {
            return None;
        }
        ethertype = u16::from_be_bytes([frame[16], frame[17]]);
        l3 = 18;
    }
    if ethertype != 0x0800 || frame.len() < l3 + 20 {
        return None;
    }
    let ihl = usize::from(frame[l3] & 0x0f) * 4;
    if ihl < 20 || frame.len() < l3 + ihl {
        return None;
    }
    if frame[l3 + 9] != 17 {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([frame[l3 + 2], frame[l3 + 3]]));
    if total_len < ihl + 8 || frame.len() < l3 + total_len {
        return None;
    }
    let flags_fragment = u16::from_be_bytes([frame[l3 + 6], frame[l3 + 7]]);
    if flags_fragment & 0x3fff != 0 {
        return None;
    }
    let udp = l3 + ihl;
    let udp_len = usize::from(u16::from_be_bytes([frame[udp + 4], frame[udp + 5]]));
    if udp_len < 8 || udp + udp_len > l3 + total_len {
        return None;
    }
    Some(&frame[udp + 8..udp + udp_len])
}

fn put_u16_le(out: &mut [u8], off: usize, value: u16) {
    out[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32_le(out: &mut [u8], off: usize, value: u32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_i32_le(out: &mut [u8], off: usize, value: i32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64_le(out: &mut [u8], off: usize, value: u64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_i64_le(out: &mut [u8], off: usize, value: i64) {
    out[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_u16_be_vec(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32_be_vec(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64_be_vec(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder_eobi::EobiSbeDecoder;
    use crate::decoder_fast::FastEmdiDecoder;
    use crate::decoder_itch::Itch50Decoder;
    use crate::parser::MessageDecoder;

    #[test]
    fn eobi_fixture_decodes_events_and_hashes() {
        let cfg = FixtureConfig {
            packet_count: 16,
            messages_per_packet: 2,
            ..FixtureConfig::default()
        };
        let fixtures = BenchmarkFixtures::new(cfg);
        let decoder = EobiSbeDecoder::new();
        let mut events = Vec::new();
        for payload in &fixtures.eobi_packets {
            decoder.decode_messages(payload, &mut events);
        }
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Add { .. })));
        let hash = state_hash_after_events(cfg, &fixtures.events);
        assert_eq!(hash, fixtures.expected_state_hash);
    }

    #[test]
    fn itch_and_fast_fixtures_decode_events() {
        let cfg = FixtureConfig {
            instruments: 4,
            orders_per_instrument: 8,
            ..FixtureConfig::default()
        };
        let fixtures = BenchmarkFixtures::new(cfg);

        let itch = Itch50Decoder::new();
        let mut itch_events = Vec::new();
        itch.decode_messages(&fixtures.itch_payload, &mut itch_events);
        assert!(itch_events
            .iter()
            .any(|event| matches!(event, Event::Add { .. })));

        let fast = FastEmdiDecoder::new();
        let mut fast_events = Vec::new();
        fast.decode_messages(&fixtures.fast_payload, &mut fast_events);
        assert!(fast_events
            .iter()
            .any(|event| matches!(event, Event::Add { .. })));
    }

    #[test]
    fn key_value_report_roundtrips_escaped_values() {
        let line = format_kv_line([
            ("profile", "local-core"),
            ("cpu", "Xeon Gold = noisy"),
            ("note", "line\nbreak"),
        ]);
        let parsed = parse_kv_line(&line).unwrap();
        assert_eq!(parsed.get("profile").unwrap(), "local-core");
        assert_eq!(parsed.get("cpu").unwrap(), "Xeon Gold = noisy");
        assert_eq!(parsed.get("note").unwrap(), "line\nbreak");
    }
}
