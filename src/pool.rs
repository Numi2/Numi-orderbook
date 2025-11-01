// src/pool.rs
use bytes::BytesMut;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;
use std::slice;

pub struct PacketPool {
    inner: Arc<ArrayQueue<BytesMut>>,
    max_packet_size: usize,
}

impl PacketPool {
    pub fn new(pool_size: usize, max_packet_size: usize) -> anyhow::Result<Self> {
        let q = Arc::new(ArrayQueue::new(pool_size));
        // Pre-allocate the entire pool to warm caches and avoid runtime allocations
        let prealloc = pool_size;
        for _ in 0..prealloc {
            let _ = q.push(BytesMut::with_capacity(max_packet_size));
        }
        Ok(Self { inner: q, max_packet_size })
    }

    #[inline]
    pub fn get(&self) -> BytesMut {
        if let Some(mut b) = self.inner.pop() {
            b.truncate(0);
            b
        } else {
            BytesMut::with_capacity(self.max_packet_size)
        }
    }

    #[inline]
    pub fn put(&self, mut buf: BytesMut) {
        buf.truncate(0);
        let _ = self.inner.push(buf);
    }
}

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsKind { None = 0, Sw = 1, HwSys = 2, HwRaw = 3 }

#[derive(Debug)]
pub enum PktBuf {
    Bytes(BytesMut),
    #[allow(dead_code)]
    Umem { ptr: *mut u8, len: usize, frame_idx: u32 },
}

#[derive(Debug)]
pub struct Pkt {
    pub buf: PktBuf,
    pub len: usize,
    pub seq: u64,
    pub ts_nanos: u64,
    pub chan: u8,
    pub _ts_kind: TsKind,
    /// Timestamp when merge forwarded the packet to decode queue
    pub merge_emit_ns: u64,
}

// Safety: Packet buffers are transferred across threads via SPSC queues.
// BytesMut is Send. The UMEM pointer represents a frame owned by the runtime
// which is reclaimed out of band; we treat it as Send here.
unsafe impl Send for Pkt {}

impl Pkt {
    #[inline]
    pub fn payload(&self) -> &[u8] {
        match &self.buf {
            PktBuf::Bytes(b) => &b[..self.len],
            PktBuf::Umem { ptr, len, .. } => unsafe { slice::from_raw_parts(*ptr as *const u8, *len) },
        }
    }

    #[inline]
    pub fn recycle(self, pool: &PacketPool) {
        match self.buf {
            PktBuf::Bytes(b) => pool.put(b),
            PktBuf::Umem { .. } => { /* TODO: return to UMEM completion ring */ }
        }
    }
}


