use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        for _ in 0..cap { v.push(UnsafeCell::new(MaybeUninit::uninit())); }
        Self { buf: v, mask: cap - 1, head: Al64(AtomicUsize::new(0)), tail: Al64(AtomicUsize::new(0)) }
    }

    #[inline]
    pub fn push(&self, value: T) -> Result<(), T> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) > self.mask { return Err(value); }
        let idx = head & self.mask;
        unsafe { (*self.buf[idx].get()).write(value); }
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    #[inline]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail == head { return None; }
        let idx = tail & self.mask;
        let v = unsafe { (*self.buf[idx].get()).assume_init_read() };
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }
}



