//! Small overwrite-on-full buffers for lossy bridge egress paths.

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::Deque;

pub struct OverwriteQueue<T, const N: usize> {
    queue: Mutex<CriticalSectionRawMutex, Deque<T, N>>,
    ready: Signal<CriticalSectionRawMutex, ()>,
}

impl<T, const N: usize> OverwriteQueue<T, N> {
    pub const fn new() -> Self {
        Self {
            queue: Mutex::new(Deque::new()),
            ready: Signal::new(),
        }
    }

    pub fn push_overwrite(&self, item: T) {
        unsafe {
            self.queue.lock_mut(|queue| {
                if queue.is_full() {
                    let _ = queue.pop_front();
                }
                let _ = queue.push_back(item);
            });
        }
        self.ready.signal(());
    }

    pub fn try_pop(&self) -> Option<T> {
        unsafe { self.queue.lock_mut(|queue| queue.pop_front()) }
    }

    pub async fn pop(&self) -> T {
        loop {
            if let Some(item) = self.try_pop() {
                return item;
            }
            self.ready.wait().await;
        }
    }
}

pub struct OverwriteByteRing<const N: usize> {
    queue: Deque<u8, N>,
}

impl<const N: usize> OverwriteByteRing<N> {
    pub const fn new() -> Self {
        Self {
            queue: Deque::new(),
        }
    }

    pub fn push_overwrite_slice(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.queue.is_full() {
                let _ = self.queue.pop_front();
            }
            let _ = self.queue.push_back(byte);
        }
    }

    pub fn pop_into(&mut self, buf: &mut [u8]) -> usize {
        let mut count = 0usize;
        while count < buf.len() {
            match self.queue.pop_front() {
                Some(byte) => {
                    buf[count] = byte;
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
