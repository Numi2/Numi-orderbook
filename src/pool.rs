// src/pool.rs
use bytes::BytesMut;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;

pub struct PacketPool {
    inner: Arc<ArrayQueue<BytesMut>>,
    max_packet_size: usize,
}

impl PacketPool {
    pub fn new(pool_size: usize, max_packet_size: usize) -> anyhow::Result<Self> {
        let q = Arc::new(ArrayQueue::new(pool_size));
        // Pre-allocate and touch the entire pool so pages are faulted in during startup,
        // not on the RX hot path.
        let prealloc = pool_size;
        for _ in 0..prealloc {
            let _ = q.push(Self::preallocated_buffer(max_packet_size));
        }
        crate::metrics::set_packet_pool_preallocated_bytes(
            pool_size.saturating_mul(max_packet_size),
        );
        Ok(Self {
            inner: q,
            max_packet_size,
        })
    }

    fn preallocated_buffer(max_packet_size: usize) -> BytesMut {
        let mut b = BytesMut::with_capacity(max_packet_size);
        if max_packet_size > 0 {
            b.resize(max_packet_size, 0);
            b.truncate(0);
        }
        b
    }

    #[inline]
    pub fn get(&self) -> BytesMut {
        if let Some(mut b) = self.inner.pop() {
            b.truncate(0);
            b
        } else {
            crate::metrics::inc_packet_pool_miss();
            BytesMut::with_capacity(self.max_packet_size)
        }
    }

    #[inline]
    pub fn put(&self, mut buf: BytesMut) {
        buf.truncate(0);
        if self.inner.push(buf).is_err() {
            crate::metrics::inc_packet_pool_return_drop();
        }
    }

    #[inline]
    pub fn available(&self) -> usize {
        self.inner.len()
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsKind {
    None = 0,
    Sw = 1,
    HwSys = 2,
    HwRaw = 3,
}

#[derive(Debug)]
pub enum PktBuf {
    Bytes(BytesMut),
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

impl Pkt {
    #[inline]
    pub fn payload(&self) -> &[u8] {
        match &self.buf {
            PktBuf::Bytes(b) => &b[..self.len],
        }
    }

    #[inline]
    pub fn recycle(self, pool: &PacketPool) {
        match self.buf {
            PktBuf::Bytes(b) => pool.put(b),
        }
    }
}
