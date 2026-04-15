use bytes::{Bytes, BytesMut};
use hashbrown::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::codec_raw::{self, FrameHeaderV1};
use crate::util::now_nanos;
use zerocopy::AsBytes;

#[derive(Clone)]
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
}

impl Bus {
    pub fn new(capacity_frames: usize) -> Self {
        let ring = Ring {
            buf: VecDeque::with_capacity(capacity_frames),
            cap: capacity_frames,
            next_global: 0,
        };
        let inner = Inner {
            ring: Mutex::new(ring),
            cv: Condvar::new(),
            per_instr_seq: Mutex::new(HashMap::new()),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn publisher(&self) -> Publisher {
        Publisher {
            inner: self.inner.clone(),
        }
    }
    pub fn subscribe(&self) -> Subscription {
        let next = self.inner.ring.lock().unwrap().next_global;
        Subscription {
            inner: self.inner.clone(),
            next_global: next,
        }
    }
}

impl Publisher {
    #[inline]
    pub fn publish_raw(
        &self,
        message_type: u16,
        channel_id: u32,
        instrument_id: u64,
        sequence: u64,
        payload: &[u8],
    ) -> u64 {
        let header_len = std::mem::size_of::<FrameHeaderV1>();
        let mut frame = BytesMut::with_capacity(header_len + payload.len());
        frame.resize(header_len, 0);
        frame.extend_from_slice(payload);

        let mut ring = self.inner.ring.lock().unwrap();
        let g = ring.next_global;
        ring.next_global = g.wrapping_add(1);
        let hdr = FrameHeaderV1 {
            magic: codec_raw::MAGIC,
            version: codec_raw::VERSION_V1,
            codec: codec_raw::codec::RAW_V1,
            message_type,
            channel_id,
            instrument_id,
            sequence,
            global_sequence: g,
            send_time_ns: now_nanos(),
            payload_len: payload.len() as u32,
        };
        frame[..header_len].copy_from_slice(hdr.as_bytes());
        let bytes = frame.freeze();
        if ring.buf.len() == ring.cap {
            ring.buf.pop_front();
        }
        ring.buf.push_back((g, bytes));
        drop(ring);
        self.inner.cv.notify_all();
        g
    }

    #[inline]
    pub fn next_seq_for_instrument(&self, instrument_id: u64) -> u64 {
        let mut m = self.inner.per_instr_seq.lock().unwrap();
        let e = m.entry(instrument_id).or_insert(1);
        let seq = *e;
        *e = e.wrapping_add(1);
        seq
    }

    #[inline]
    pub fn next_global_sequence(&self) -> u64 {
        self.inner.ring.lock().unwrap().next_global
    }
}

impl Subscription {
    pub fn set_cursor_to_tail(&mut self) {
        let r = self.inner.ring.lock().unwrap();
        self.next_global = r.next_global;
    }

    pub fn set_cursor(&mut self, global_seq: u64) {
        self.next_global = global_seq;
    }

    pub fn cursor_available(&self) -> bool {
        let guard = self.inner.ring.lock().unwrap();
        if self.next_global == guard.next_global {
            return true;
        }
        if self.next_global > guard.next_global || guard.buf.is_empty() {
            return false;
        }
        let oldest_g = guard.next_global.saturating_sub(guard.buf.len() as u64);
        self.next_global >= oldest_g
    }

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
            if offset >= guard.buf.len() {
                return Err(RecvError::Gap {
                    from: self.next_global,
                    to: guard.next_global.saturating_sub(1),
                });
            }
            let (_g, bytes) = guard.buf[offset].clone();
            self.next_global = self.next_global.wrapping_add(1);
            return Ok(bytes);
        }
    }

    /// Receive the next frame, returning `Ok(None)` when no frame arrives before `timeout`.
    pub fn recv_next_timeout(&mut self, timeout: Duration) -> Result<Option<Bytes>, RecvError> {
        let start = Instant::now();
        let mut guard = self.inner.ring.lock().unwrap();
        loop {
            if !guard.buf.is_empty() && self.next_global < guard.next_global {
                let oldest_g = guard.next_global.saturating_sub(guard.buf.len() as u64);
                if self.next_global < oldest_g {
                    let from = self.next_global;
                    let to = oldest_g.saturating_sub(1);
                    return Err(RecvError::Gap { from, to });
                }
                let offset = (self.next_global - oldest_g) as usize;
                if offset >= guard.buf.len() {
                    return Err(RecvError::Gap {
                        from: self.next_global,
                        to: guard.next_global.saturating_sub(1),
                    });
                }
                let (_g, bytes) = guard.buf[offset].clone();
                self.next_global = self.next_global.wrapping_add(1);
                return Ok(Some(bytes));
            }

            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(elapsed);
            let (next_guard, _timeout) = self.inner.cv.wait_timeout(guard, remaining).unwrap();
            guard = next_guard;
        }
    }

    /// Approximate lag in frames behind the current tail.
    #[inline]
    pub fn lag(&self) -> u64 {
        let guard = self.inner.ring.lock().unwrap();
        if guard.buf.is_empty() {
            return 0;
        }
        let oldest_g = guard.next_global.saturating_sub(guard.buf.len() as u64);
        let cursor = self.next_global.max(oldest_g);
        guard.next_global.saturating_sub(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_created_before_snapshot_receives_frames_published_during_snapshot() {
        let bus = Bus::new(8);
        let publisher = bus.publisher();
        let mut sub = bus.subscribe();

        publisher.publish_raw(100, 0, 42, 1, b"one");
        publisher.publish_raw(100, 0, 42, 2, b"two");

        let first = sub.recv_next_blocking().unwrap();
        let second = sub.recv_next_blocking().unwrap();
        assert!(first.ends_with(b"one"));
        assert!(second.ends_with(b"two"));
        assert_eq!(sub.lag(), 0);
    }

    #[test]
    fn published_frames_include_global_replay_cursor() {
        let bus = Bus::new(8);
        let publisher = bus.publisher();
        let mut sub = bus.subscribe();

        publisher.publish_raw(100, 0, 42, 1, b"one");
        publisher.publish_raw(100, 0, 42, 2, b"two");

        let first = sub.recv_next_blocking().unwrap();
        let second = sub.recv_next_blocking().unwrap();
        assert_eq!(le_u64(&first[28..36]), 0);
        assert_eq!(le_u64(&second[28..36]), 1);
    }

    #[test]
    fn stale_subscription_reports_exact_evicted_global_range() {
        let bus = Bus::new(2);
        let publisher = bus.publisher();
        let mut sub = bus.subscribe();

        publisher.publish_raw(100, 0, 1, 1, b"a");
        publisher.publish_raw(100, 0, 1, 2, b"b");
        publisher.publish_raw(100, 0, 1, 3, b"c");

        match sub.recv_next_blocking() {
            Err(RecvError::Gap { from, to }) => {
                assert_eq!((from, to), (0, 0));
            }
            Ok(_) => panic!("expected evicted cursor gap"),
        }
    }

    #[test]
    fn cursor_available_tracks_retained_global_window() {
        let bus = Bus::new(2);
        let publisher = bus.publisher();
        let mut sub = bus.subscribe();

        assert!(sub.cursor_available());
        publisher.publish_raw(100, 0, 1, 1, b"a");
        publisher.publish_raw(100, 0, 1, 2, b"b");
        publisher.publish_raw(100, 0, 1, 3, b"c");

        sub.set_cursor(0);
        assert!(!sub.cursor_available());
        sub.set_cursor(1);
        assert!(sub.cursor_available());
        sub.set_cursor(3);
        assert!(sub.cursor_available());
        sub.set_cursor(4);
        assert!(!sub.cursor_available());
    }

    #[test]
    fn per_instrument_sequence_is_monotonic_and_independent() {
        let bus = Bus::new(8);
        let publisher = bus.publisher();

        assert_eq!(publisher.next_seq_for_instrument(10), 1);
        assert_eq!(publisher.next_seq_for_instrument(10), 2);
        assert_eq!(publisher.next_seq_for_instrument(20), 1);
        assert_eq!(publisher.next_seq_for_instrument(10), 3);
    }

    #[test]
    fn timeout_receive_returns_none_without_advancing_cursor() {
        let bus = Bus::new(8);
        let publisher = bus.publisher();
        let mut sub = bus.subscribe();

        assert_eq!(
            sub.recv_next_timeout(Duration::from_millis(0)).unwrap(),
            None
        );
        publisher.publish_raw(100, 0, 1, 1, b"x");
        let frame = sub
            .recv_next_timeout(Duration::from_millis(0))
            .unwrap()
            .unwrap();
        assert!(frame.ends_with(b"x"));
    }

    fn le_u64(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes.try_into().unwrap())
    }
}
