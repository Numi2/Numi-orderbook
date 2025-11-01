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
        // Pre-allocate a portion of the pool to warm caches and reduce first-packet jitters
        let prealloc = pool_size.min(1024);
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

#[derive(Debug)]
pub struct Pkt {
    pub buf: BytesMut,
    pub len: usize,
    pub seq: u64,
    pub ts_nanos: u64,
    pub chan: u8,
}


