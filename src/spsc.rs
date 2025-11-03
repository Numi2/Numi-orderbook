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

    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
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
