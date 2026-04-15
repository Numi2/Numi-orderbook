use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Conservative upper bound for adaptive queue batches.
pub const DEFAULT_BATCH_CAP: usize = 32;

#[repr(align(64))]
struct Al64<T>(T);

pub struct SpscQueue<T> {
    buf: Vec<UnsafeCell<MaybeUninit<T>>>,
    mask: usize,
    head: Al64<AtomicUsize>,
    tail: Al64<AtomicUsize>,
}

unsafe impl<T: Send> Send for SpscQueue<T> {}
unsafe impl<T: Send> Sync for SpscQueue<T> {}

impl<T> SpscQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two().max(2);
        let mut v = Vec::with_capacity(cap);
        for _ in 0..cap {
            v.push(UnsafeCell::new(MaybeUninit::uninit()));
        }
        Self {
            buf: v,
            mask: cap - 1,
            head: Al64(AtomicUsize::new(0)),
            tail: Al64(AtomicUsize::new(0)),
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    #[inline]
    pub fn push(&self, value: T) -> Result<(), T> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) > self.mask {
            return Err(value);
        }
        let idx = head & self.mask;
        unsafe {
            (*self.buf[idx].get()).write(value);
        }
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pushes up to `max_items` from the front of `values`, publishing the batch with one
    /// release-store. Unpushed values remain in order in `values`.
    #[inline]
    pub fn push_batch(&self, values: &mut Vec<T>, max_items: usize) -> usize {
        let requested = values.len().min(max_items);
        if requested == 0 {
            return 0;
        }

        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        let occupied = head.wrapping_sub(tail);
        let writable = self.capacity().saturating_sub(occupied).min(requested);
        if writable == 0 {
            return 0;
        }

        for (offset, value) in values.drain(..writable).enumerate() {
            let idx = head.wrapping_add(offset) & self.mask;
            unsafe {
                (*self.buf[idx].get()).write(value);
            }
        }
        self.head
            .0
            .store(head.wrapping_add(writable), Ordering::Release);
        writable
    }

    #[inline]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = tail & self.mask;
        let v = unsafe { (*self.buf[idx].get()).assume_init_read() };
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    /// Pops up to `max_items` into `out`, retiring the batch with one release-store.
    #[inline]
    pub fn pop_batch(&self, out: &mut Vec<T>, max_items: usize) -> usize {
        if max_items == 0 {
            return 0;
        }

        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        let readable = head.wrapping_sub(tail).min(max_items);
        if readable == 0 {
            return 0;
        }

        out.reserve(readable);
        for offset in 0..readable {
            let idx = tail.wrapping_add(offset) & self.mask;
            let value = unsafe { (*self.buf[idx].get()).assume_init_read() };
            out.push(value);
        }
        self.tail
            .0
            .store(tail.wrapping_add(readable), Ordering::Release);
        readable
    }

    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }

    #[inline]
    pub fn spare_capacity(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        self.capacity().saturating_sub(head.wrapping_sub(tail))
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Attempt to push with bounded spinning/yielding before giving up.
    #[inline]
    pub fn push_with_backoff(&self, mut value: T, max_spins: u32) -> Result<(), T> {
        let mut spins: u32 = 0;
        loop {
            match self.push(value) {
                Ok(()) => return Ok(()),
                Err(v) => {
                    value = v;
                    if spins >= max_spins {
                        return Err(value);
                    }
                    crate::util::spin_wait(32);
                    if (spins & 0x3f) == 0 {
                        std::thread::yield_now();
                    }
                    spins = spins.wrapping_add(1);
                }
            }
        }
    }

    /// Push, blocking in userspace (spin/yield) until space is available.
    /// Suitable only when the producer must not drop values.
    #[inline]
    pub fn push_blocking(&self, mut value: T) {
        let mut spins: u32 = 0;
        loop {
            match self.push(value) {
                Ok(()) => return,
                Err(v) => {
                    value = v;
                    crate::util::spin_wait(64);
                    if (spins & 0xff) == 0 {
                        std::thread::yield_now();
                    }
                    spins = spins.wrapping_add(1);
                }
            }
        }
    }
}

impl<T> Drop for SpscQueue<T> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AdaptiveBatchCap {
    min: usize,
    max: usize,
    current: usize,
}

impl AdaptiveBatchCap {
    pub fn new(min: usize, max: usize) -> Self {
        let min = min.max(1);
        let max = max.max(min);
        Self {
            min,
            max,
            current: min,
        }
    }

    #[inline]
    pub fn current(&self) -> usize {
        self.current
    }

    #[inline]
    pub fn max(&self) -> usize {
        self.max
    }

    #[inline]
    pub fn record(&mut self, attempted: usize, completed: usize) {
        if completed == 0 {
            self.current = self.min;
        } else if completed >= self.current && attempted >= self.current {
            self.current = self.current.saturating_mul(2).min(self.max);
        } else if completed.saturating_mul(2) < self.current {
            self.current = (self.current / 2).max(self.min);
        }
    }

    #[inline]
    pub fn reset(&mut self) {
        self.current = self.min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn push_batch_keeps_rejected_suffix_ordered() {
        let q = SpscQueue::new(4);
        let mut values = vec![1, 2, 3, 4, 5];

        assert_eq!(q.push_batch(&mut values, 3), 3);
        assert_eq!(values, vec![4, 5]);
        assert_eq!(q.len(), 3);

        let mut out = Vec::new();
        assert_eq!(q.pop_batch(&mut out, 2), 2);
        assert_eq!(out, vec![1, 2]);

        assert_eq!(q.push_batch(&mut values, DEFAULT_BATCH_CAP), 2);
        assert!(values.is_empty());

        assert_eq!(q.pop(), Some(3));
        out.clear();
        assert_eq!(q.pop_batch(&mut out, DEFAULT_BATCH_CAP), 2);
        assert_eq!(out, vec![4, 5]);
        assert!(q.is_empty());
    }

    #[test]
    fn push_batch_stops_at_available_capacity() {
        let q = SpscQueue::new(2);
        assert!(q.push(1).is_ok());
        assert!(q.push(2).is_ok());

        let mut values = vec![3, 4];
        assert_eq!(q.push_batch(&mut values, DEFAULT_BATCH_CAP), 0);
        assert_eq!(values, vec![3, 4]);

        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.push_batch(&mut values, DEFAULT_BATCH_CAP), 1);
        assert_eq!(values, vec![4]);
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
    }

    #[test]
    fn drop_releases_queued_values() {
        struct Counted(Arc<AtomicUsize>);

        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        {
            let q = SpscQueue::new(4);
            assert!(q.push(Counted(dropped.clone())).is_ok());
            assert!(q.push(Counted(dropped.clone())).is_ok());
        }

        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn adaptive_batch_cap_grows_and_shrinks() {
        let mut cap = AdaptiveBatchCap::new(1, 8);

        assert_eq!(cap.current(), 1);
        cap.record(1, 1);
        assert_eq!(cap.current(), 2);
        cap.record(2, 2);
        assert_eq!(cap.current(), 4);
        cap.record(4, 4);
        assert_eq!(cap.current(), 8);
        cap.record(8, 1);
        assert_eq!(cap.current(), 4);
        cap.record(4, 0);
        assert_eq!(cap.current(), 1);
    }
}
