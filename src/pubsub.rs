use bytes::{Bytes, BytesMut};
use hashbrown::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::codec_raw::{self, FrameHeaderV1};
use crate::util::now_nanos;

pub struct Bus {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub struct Publisher {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub struct Subscription {
    inner: Arc<Inner>,
    next_global: u64,
}

struct Inner {
    // ring of (global_seq, frame)
    ring: Mutex<Ring>,
    cv: Condvar,
    // per-instrument sequence state
    per_instr_seq: Mutex<HashMap<u64, u64>>, // instrument_id -> next_seq
}

struct Ring {
    buf: VecDeque<(u64, Bytes)>,
    cap: usize,
    next_global: u64,
}

#[derive(Debug)]
pub enum RecvError {
    Gap { from: u64, to: u64 },
    Closed,
}

impl Bus {
    pub fn new(capacity_frames: usize) -> Self {
        let ring = Ring { buf: VecDeque::with_capacity(capacity_frames), cap: capacity_frames, next_global: 0 };
        let inner = Inner { ring: Mutex::new(ring), cv: Condvar::new(), per_instr_seq: Mutex::new(HashMap::new()) };
        Self { inner: Arc::new(inner) }
    }

    pub fn publisher(&self) -> Publisher { Publisher { inner: self.inner.clone() } }
    pub fn subscribe(&self) -> Subscription {
        let next = self.inner.ring.lock().unwrap().next_global;
        Subscription { inner: self.inner.clone(), next_global: next }
    }
}

impl Publisher {
    #[inline]
    pub fn publish_raw(&self, message_type: u16, channel_id: u32, instrument_id: u64, sequence: u64, payload: &[u8]) {
        let mut frame = BytesMut::with_capacity(std::mem::size_of::<FrameHeaderV1>() + payload.len());
        let hdr = FrameHeaderV1 {
            magic: codec_raw::MAGIC,
            version: codec_raw::VERSION_V1,
            codec: codec_raw::codec::RAW_V1,
            message_type,
            channel_id,
            instrument_id,
            sequence,
            send_time_ns: now_nanos(),
            payload_len: payload.len() as u32,
        };
        frame.extend_from_slice(hdr.as_bytes());
        frame.extend_from_slice(payload);
        let bytes = frame.freeze();
        let mut ring = self.inner.ring.lock().unwrap();
        let g = ring.next_global;
        ring.next_global = g.wrapping_add(1);
        if ring.buf.len() == ring.cap { ring.buf.pop_front(); }
        ring.buf.push_back((g, bytes));
        drop(ring);
        self.inner.cv.notify_all();
    }

    #[inline]
    pub fn next_seq_for_instrument(&self, instrument_id: u64) -> u64 {
        let mut m = self.inner.per_instr_seq.lock().unwrap();
        let e = m.entry(instrument_id).or_insert(1);
        let seq = *e;
        *e = e.wrapping_add(1);
        seq
    }
}

impl Subscription {
    pub fn set_cursor_to_tail(&mut self) {
        let r = self.inner.ring.lock().unwrap();
        self.next_global = r.next_global;
    }

    pub fn set_cursor(&mut self, global_seq: u64) { self.next_global = global_seq; }

    pub fn recv_next_blocking(&mut self) -> Result<Bytes, RecvError> {
        let mut guard = self.inner.ring.lock().unwrap();
        loop {
            // If nothing new, wait
            if guard.buf.is_empty() || self.next_global >= guard.next_global {
                guard = self.inner.cv.wait(guard).unwrap();
                continue;
            }

            // Oldest global in buffer
            let oldest_g = guard.next_global.saturating_sub(guard.buf.len() as u64);
            if self.next_global < oldest_g {
                let from = self.next_global;
                let to = oldest_g.saturating_sub(1);
                return Err(RecvError::Gap { from, to });
            }
            let offset = (self.next_global - oldest_g) as usize;
            if offset >= guard.buf.len() { return Err(RecvError::Gap { from: self.next_global, to: guard.next_global.saturating_sub(1) }); }
            let (_g, bytes) = guard.buf[offset].clone();
            self.next_global = self.next_global.wrapping_add(1);
            return Ok(bytes);
        }
    }
}


