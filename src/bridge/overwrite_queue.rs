//! Small overwrite-on-full buffers for lossy bridge egress paths.

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::Deque;
use portable_atomic::{AtomicU32, Ordering};
use core::fmt::Write;

// Aggregate lifetime counters across mailbox queues and UART rings. These are
// diagnostic only; eviction remains an explicit bounded, lossy policy.
static MAILBOX_DROPPED: AtomicU32 = AtomicU32::new(0);
static UART_DROPPED: AtomicU32 = AtomicU32::new(0);
static REJECTED: AtomicU32 = AtomicU32::new(0);
static MAILBOX_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
static UART_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
pub fn render_diagnostics() -> heapless::String<192> {
    let mut out = heapless::String::new();
    let _ = write!(out, "queues mailbox_drop={} uart_drop={} reject={} mailbox_peak={} uart_peak={}",
        MAILBOX_DROPPED.load(Ordering::Relaxed), UART_DROPPED.load(Ordering::Relaxed),
        REJECTED.load(Ordering::Relaxed), MAILBOX_HIGH_WATER.load(Ordering::Relaxed),
        UART_HIGH_WATER.load(Ordering::Relaxed));
    out
}

// I2C packets carry a 4 KiB inline payload, so keep this depth bounded to avoid
// exhausting RP2040 RAM under burst traffic.
pub const I2C_PACKET_QUEUE_DEPTH: usize = 8;
pub const I2C_PACKET_QUEUE_BYTES: usize = 8192;
pub const SPI_PACKET_QUEUE_DEPTH: usize = 32;
pub const SPI_PACKET_QUEUE_BYTES: usize = 8192;

pub trait QueueItem {
    fn queued_len(&self) -> usize;
}

struct QueueState<T, const N: usize> {
    queue: Deque<T, N>,
    queued_bytes: usize,
}

impl<T, const N: usize> QueueState<T, N> {
    const fn new() -> Self {
        Self {
            queue: Deque::new(),
            queued_bytes: 0,
        }
    }
}

pub struct OverwriteQueue<T, const N: usize, const MAX_BYTES: usize> {
    state: Mutex<CriticalSectionRawMutex, QueueState<T, N>>,
    ready: Signal<CriticalSectionRawMutex, ()>,
}

impl<T: QueueItem, const N: usize, const MAX_BYTES: usize> OverwriteQueue<T, N, MAX_BYTES> {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::new()),
            ready: Signal::new(),
        }
    }

    pub fn push_overwrite(&self, item: T) -> bool {
        let item_len = item.queued_len();
        if item_len > MAX_BYTES || N == 0 {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        let pushed = unsafe {
            self.state.lock_mut(|state| {
                while state.queue.is_full()
                    || state.queued_bytes.saturating_add(item_len) > MAX_BYTES
                {
                    match state.queue.pop_front() {
                        Some(dropped) => {
                            MAILBOX_DROPPED.fetch_add(1, Ordering::Relaxed);
                            state.queued_bytes =
                                state.queued_bytes.saturating_sub(dropped.queued_len());
                        }
                        None => break,
                    }
                }

                if state.queue.push_back(item).is_ok() {
                    state.queued_bytes = state.queued_bytes.saturating_add(item_len);
                    MAILBOX_HIGH_WATER.fetch_max(state.queue.len() as u32, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            })
        };
        if pushed {
            self.ready.signal(());
        }
        pushed
    }

    pub fn try_pop(&self) -> Option<T> {
        unsafe {
            self.state.lock_mut(|state| {
                let item = state.queue.pop_front()?;
                state.queued_bytes = state.queued_bytes.saturating_sub(item.queued_len());
                Some(item)
            })
        }
    }

    pub fn try_pop_latest(&self) -> Option<T> {
        unsafe {
            self.state.lock_mut(|state| {
                let mut latest = state.queue.pop_front()?;
                while let Some(next) = state.queue.pop_front() {
                    MAILBOX_DROPPED.fetch_add(1, Ordering::Relaxed);
                    latest = next;
                }
                state.queued_bytes = 0;
                Some(latest)
            })
        }
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

struct BytePacket<const M: usize> {
    len: usize,
    data: [u8; M],
}

impl<const M: usize> BytePacket<M> {
    const fn new() -> Self {
        Self {
            len: 0,
            data: [0; M],
        }
    }

    fn from_slices(slices: &[&[u8]]) -> Self {
        let mut packet = Self::new();
        for slice in slices {
            let remaining = M.saturating_sub(packet.len);
            let copy_len = slice.len().min(remaining);
            if copy_len == 0 {
                break;
            }
            packet.data[packet.len..packet.len + copy_len].copy_from_slice(&slice[..copy_len]);
            packet.len += copy_len;
        }
        packet
    }
}

pub struct OverwriteBytePacketRing<const N: usize, const M: usize> {
    queue: Deque<BytePacket<M>, N>,
    front_offset: usize,
}

impl<const N: usize, const M: usize> OverwriteBytePacketRing<N, M> {
    pub const fn new() -> Self {
        Self {
            queue: Deque::new(),
            front_offset: 0,
        }
    }

    pub fn push_overwrite_slices(&mut self, slices: &[&[u8]]) {
        // Never silently truncate a framed stream packet.
        let total = slices.iter().try_fold(0usize, |n, s| n.checked_add(s.len()));
        if N == 0 || total.is_none_or(|n| n > M) {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let packet = BytePacket::from_slices(slices);
        if packet.len == 0 {
            return;
        }

        if self.queue.is_full() {
            if self.front_offset != 0 && self.queue.len() == 1 {
                // The head is already on the wire. Dropping its suffix would
                // corrupt framing; reject the new packet instead.
                REJECTED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            UART_DROPPED.fetch_add(1, Ordering::Relaxed);
            if self.front_offset == 0 {
                let _ = self.queue.pop_front();
            } else if self.queue.len() > 1 {
                let mut front = self.queue.pop_front();
                let _ = self.queue.pop_front();
                if let Some(front) = front.take() {
                    let _ = self.queue.push_front(front);
                }
            } else {
                let _ = self.queue.pop_back();
                self.front_offset = 0;
            }
        }

        let _ = self.queue.push_back(packet);
        UART_HIGH_WATER.fetch_max(self.queue.len() as u32, Ordering::Relaxed);
    }

    pub fn peek_into(&self, buf: &mut [u8]) -> usize {
        let mut count = 0usize;
        for (packet_index, packet) in self.queue.iter().enumerate() {
            let start = if packet_index == 0 {
                self.front_offset.min(packet.len)
            } else {
                0
            };
            let available = packet.len.saturating_sub(start);
            if available == 0 {
                continue;
            }

            let copy_len = available.min(buf.len().saturating_sub(count));
            if copy_len == 0 {
                break;
            }
            buf[count..count + copy_len].copy_from_slice(&packet.data[start..start + copy_len]);
            count += copy_len;
        }
        count
    }

    pub fn consume(&mut self, mut count: usize) {
        while count != 0 {
            let front_len = match self.queue.front() {
                Some(packet) => packet.len,
                None => {
                    self.front_offset = 0;
                    break;
                }
            };
            let remaining = front_len.saturating_sub(self.front_offset);
            if count < remaining {
                self.front_offset += count;
                break;
            }

            count = count.saturating_sub(remaining);
            let _ = self.queue.pop_front();
            self.front_offset = 0;
        }
    }

    pub fn clear(&mut self) {
        UART_DROPPED.fetch_add(self.queue.len() as u32, Ordering::Relaxed);
        self.queue.clear();
        self.front_offset = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, PartialEq)]
    struct Item(u8, usize);
    impl QueueItem for Item { fn queued_len(&self) -> usize { self.1 } }
    #[test]
    fn mailbox_enforces_count_bytes_and_fifo() {
        let q = OverwriteQueue::<Item, 2, 5>::new();
        assert!(q.push_overwrite(Item(1, 2)));
        assert!(q.push_overwrite(Item(2, 2)));
        assert!(q.push_overwrite(Item(3, 4)));
        assert_eq!(q.try_pop(), Some(Item(3, 4)));
        assert_eq!(q.try_pop(), None);
        assert!(!q.push_overwrite(Item(4, 6)));
        assert!(q.push_overwrite(Item(5, 1)));
        assert!(q.push_overwrite(Item(6, 1)));
        assert_eq!(q.try_pop(), Some(Item(5, 1)));
        assert_eq!(q.try_pop(), Some(Item(6, 1)));
    }
    #[test]
    fn uart_preserves_partially_transmitted_head() {
        let mut q = OverwriteBytePacketRing::<2, 8>::new();
        q.push_overwrite_slices(&[b"abcd"]);
        q.consume(2);
        q.push_overwrite_slices(&[b"old"]);
        q.push_overwrite_slices(&[b"new"]);
        let mut buf = [0; 16];
        let n = q.peek_into(&mut buf);
        assert_eq!(&buf[..n], b"cdnew");
        q.consume(n);
        assert!(q.is_empty());
    }
    #[test]
    fn uart_rejects_oversize_and_never_truncates_active_frame() {
        let mut q = OverwriteBytePacketRing::<1, 4>::new();
        q.push_overwrite_slices(&[b"abcde"]);
        assert!(q.is_empty());
        q.push_overwrite_slices(&[b"abcd"]);
        q.consume(1);
        q.push_overwrite_slices(&[b"new"]);
        let mut buf = [0; 8];
        let n = q.peek_into(&mut buf);
        assert_eq!(&buf[..n], b"bcd");
        assert!(render_diagnostics().contains("reject="));
    }
}
