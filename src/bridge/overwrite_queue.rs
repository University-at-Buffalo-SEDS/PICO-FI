//! Byte-backed mailboxes with explicit loss policy and backpressured network replies.

use core::fmt::Write;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::Deque;
use portable_atomic::{AtomicU32, Ordering};

// Aggregate lifetime counters across mailbox queues and UART rings. These are
// diagnostic only. Bus-originated mailboxes may evict; network replies and
// full UART rings preserve accepted frames.
static MAILBOX_DROPPED: AtomicU32 = AtomicU32::new(0);
static UART_DROPPED: AtomicU32 = AtomicU32::new(0);
static REJECTED: AtomicU32 = AtomicU32::new(0);
static MAILBOX_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
static UART_HIGH_WATER: AtomicU32 = AtomicU32::new(0);
pub fn render_diagnostics() -> heapless::String<192> {
    let mut out = heapless::String::new();
    let _ = write!(
        out,
        "queues mailbox_drop={} uart_drop={} reject={} mailbox_peak={} uart_peak={}",
        MAILBOX_DROPPED.load(Ordering::Relaxed),
        UART_DROPPED.load(Ordering::Relaxed),
        REJECTED.load(Ordering::Relaxed),
        MAILBOX_HIGH_WATER.load(Ordering::Relaxed),
        UART_HIGH_WATER.load(Ordering::Relaxed)
    );
    out
}

// Store only actual bytes, rather than N inline 4 KiB packet objects. This
// absorbs short telemetry bursts while using less RP2040 RAM than eight slots.
pub const I2C_PACKET_QUEUE_DEPTH: usize = 128;
pub const I2C_PACKET_QUEUE_BYTES: usize = 8192;
pub const SPI_PACKET_QUEUE_DEPTH: usize = 32;
pub const SPI_PACKET_QUEUE_BYTES: usize = 8192;

pub trait QueueItem {
    fn queued_len(&self) -> usize;
    fn copy_queued_slice(&self, start: usize, target: &mut [u8]);
    fn from_queued_slices(first: &[u8], second: &[u8]) -> Option<Self>
    where
        Self: Sized;
}

struct QueueState<const N: usize, const B: usize> {
    lengths: Deque<usize, N>,
    bytes: [u8; B],
    head: usize,
    used: usize,
}
impl<const N: usize, const B: usize> QueueState<N, B> {
    const fn new() -> Self {
        Self {
            lengths: Deque::new(),
            bytes: [0; B],
            head: 0,
            used: 0,
        }
    }
    fn fits(&self, len: usize) -> bool {
        len > 0 && !self.lengths.is_full() && len <= B.saturating_sub(self.used)
    }
    fn discard_front(&mut self) -> Option<()> {
        let len = self.lengths.pop_front()?;
        self.head = (self.head + len) % B;
        self.used -= len;
        Some(())
    }
    fn insert<T: QueueItem>(&mut self, item: &T) {
        let len = item.queued_len();
        let tail = (self.head + self.used) % B;
        let first = len.min(B - tail);
        item.copy_queued_slice(0, &mut self.bytes[tail..tail + first]);
        item.copy_queued_slice(first, &mut self.bytes[..len - first]);
        self.lengths.push_back(len).ok().unwrap();
        self.used += len;
        MAILBOX_HIGH_WATER.fetch_max(self.lengths.len() as u32, Ordering::Relaxed);
    }
}

pub struct OverwriteQueue<T, const N: usize, const MAX_BYTES: usize> {
    state: Mutex<CriticalSectionRawMutex, QueueState<N, MAX_BYTES>>,
    ready: Signal<CriticalSectionRawMutex, ()>,
    space: Signal<CriticalSectionRawMutex, ()>,
    item: core::marker::PhantomData<T>,
}
impl<T: QueueItem, const N: usize, const MAX_BYTES: usize> OverwriteQueue<T, N, MAX_BYTES> {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::new()),
            ready: Signal::new(),
            space: Signal::new(),
            item: core::marker::PhantomData,
        }
    }
    pub fn has_space_for(&self, len: usize) -> bool {
        unsafe { self.state.lock_mut(|state| state.fits(len)) }
    }
    pub async fn wait_for_space(&self, len: usize) {
        loop {
            if self.has_space_for(len) {
                return;
            }
            self.space.wait().await;
        }
    }
    /// Network streams must use this path: refusing new work preserves every
    /// accepted dictionary template, dependent frame and acknowledgement.
    pub fn try_push(&self, item: T) -> Result<(), T> {
        let pushed = unsafe {
            self.state.lock_mut(|state| {
                if !state.fits(item.queued_len()) {
                    return false;
                }
                state.insert(&item);
                true
            })
        };
        if pushed {
            self.ready.signal(());
            Ok(())
        } else {
            Err(item)
        }
    }
    pub async fn push(&self, mut item: T) -> Result<(), T> {
        let len = item.queued_len();
        if len == 0 || len > MAX_BYTES || N == 0 {
            return Err(item);
        }
        loop {
            self.wait_for_space(len).await;
            match self.try_push(item) {
                Ok(()) => return Ok(()),
                Err(returned) => item = returned,
            }
        }
    }
    /// Explicit lossy policy retained for bus-originated work.
    pub fn push_overwrite(&self, item: T) -> bool {
        let len = item.queued_len();
        if len == 0 || len > MAX_BYTES || N == 0 {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        unsafe {
            self.state.lock_mut(|state| {
                while !state.fits(len) {
                    if state.discard_front().is_none() {
                        return;
                    }
                    MAILBOX_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
                state.insert(&item);
            })
        };
        self.ready.signal(());
        true
    }
    pub fn try_pop(&self) -> Option<T> {
        let item = unsafe {
            self.state.lock_mut(|state| {
                let len = *state.lengths.front()?;
                let first = len.min(MAX_BYTES - state.head);
                let item = T::from_queued_slices(
                    &state.bytes[state.head..state.head + first],
                    &state.bytes[..len - first],
                );
                state.discard_front();
                item
            })
        };
        if item.is_some() {
            self.space.signal(());
        }
        item
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

pub struct BytePacketRing<const N: usize, const M: usize> {
    queue: Deque<BytePacket<M>, N>,
    front_offset: usize,
}

impl<const N: usize, const M: usize> BytePacketRing<N, M> {
    pub const fn new() -> Self {
        Self {
            queue: Deque::new(),
            front_offset: 0,
        }
    }

    /// Admit a complete framed packet without evicting accepted stream bytes.
    pub fn try_push_slices(&mut self, slices: &[&[u8]]) -> Result<(), ()> {
        let total = slices
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()));
        if N == 0 || total.is_none_or(|n| n > M) || self.queue.is_full() {
            REJECTED.fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
        let packet = BytePacket::from_slices(slices);
        if packet.len != 0 {
            self.queue.push_back(packet).map_err(|_| ())?;
            UART_HIGH_WATER.fetch_max(self.queue.len() as u32, Ordering::Relaxed);
        }
        Ok(())
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
    impl QueueItem for Item {
        fn queued_len(&self) -> usize {
            self.1
        }
        fn copy_queued_slice(&self, _: usize, target: &mut [u8]) {
            target.fill(self.0);
        }
        fn from_queued_slices(first: &[u8], second: &[u8]) -> Option<Self> {
            Some(Self(
                *first.first().or(second.first())?,
                first.len() + second.len(),
            ))
        }
    }
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
    fn blocked_network_reply_wakes_only_when_the_complete_packet_fits() {
        use core::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        let q = OverwriteQueue::<Item, 2, 5>::new();
        q.try_push(Item(1, 2)).unwrap();
        q.try_push(Item(2, 2)).unwrap();
        let mut pending = std::boxed::Box::pin(q.push(Item(3, 4)));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(pending.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(q.try_pop(), Some(Item(1, 2)));
        assert!(matches!(pending.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(q.try_pop(), Some(Item(2, 2)));
        assert!(matches!(
            pending.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(q.try_pop(), Some(Item(3, 4)));
    }
    #[test]
    fn short_frame_burst_retains_template_and_every_dependent_frame() {
        let q = OverwriteQueue::<Item, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>::new();
        // The captured old eight-slot mailbox evicted SDT templates during a
        // short burst despite leaving most of its 8 KiB byte budget unused.
        for id in 0..128 {
            assert!(q.try_push(Item(id, 32)).is_ok());
        }
        assert!(q.try_push(Item(250, 32)).is_err());
        for id in 0..128 {
            assert_eq!(q.try_pop(), Some(Item(id, 32)));
        }
        assert_eq!(q.try_pop(), None);
        assert!(core::mem::size_of_val(&q) < 10 * 1024);
    }
    #[test]
    fn packed_mailbox_wraps_and_preserves_refused_ownership() {
        let q = OverwriteQueue::<Item, 4, 11>::new();
        q.try_push(Item(1, 7)).unwrap();
        q.try_push(Item(2, 3)).unwrap();
        assert_eq!(q.try_push(Item(3, 2)), Err(Item(3, 2)));
        assert_eq!(q.try_pop(), Some(Item(1, 7)));
        q.try_push(Item(4, 8)).unwrap();
        assert_eq!(q.try_pop(), Some(Item(2, 3)));
        assert_eq!(q.try_pop(), Some(Item(4, 8)));
        assert_eq!(q.try_pop(), None);
        assert!(q.try_push(Item(0, 0)).is_err());
        assert!(q.try_push(Item(0, 12)).is_err());
    }
    #[test]
    fn uart_full_queue_never_invalidates_a_staged_transmit_chunk() {
        let mut q = BytePacketRing::<2, 8>::new();
        q.try_push_slices(&[b"header", b"12"]).unwrap();
        q.try_push_slices(&[b"second"]).unwrap();
        let mut before = [0; 16];
        let n = q.peek_into(&mut before);
        assert!(q.try_push_slices(&[b"command"]).is_err());
        let mut after = [0; 16];
        assert_eq!(q.peek_into(&mut after), n);
        assert_eq!(
            before, after,
            "even an unstarted staged frame must stay byte-identical"
        );
        q.consume(3);
        assert!(q.try_push_slices(&[b"command"]).is_err());
        let n = q.peek_into(&mut after);
        assert_eq!(&after[..n], b"der12second");
    }

    #[test]
    fn uart_preserves_partially_transmitted_head() {
        let mut q = BytePacketRing::<2, 8>::new();
        q.try_push_slices(&[b"abcd"]).unwrap();
        q.consume(2);
        q.try_push_slices(&[b"old"]).unwrap();
        assert!(q.try_push_slices(&[b"new"]).is_err());
        let mut buf = [0; 16];
        let n = q.peek_into(&mut buf);
        assert_eq!(&buf[..n], b"cdold");
        q.consume(n);
        assert!(q.is_empty());
    }
    #[test]
    fn uart_rejects_oversize_and_never_truncates_active_frame() {
        let mut q = BytePacketRing::<1, 4>::new();
        assert!(q.try_push_slices(&[b"abcde"]).is_err());
        assert!(q.is_empty());
        q.try_push_slices(&[b"abcd"]).unwrap();
        q.consume(1);
        assert!(q.try_push_slices(&[b"new"]).is_err());
        let mut buf = [0; 8];
        let n = q.peek_into(&mut buf);
        assert_eq!(&buf[..n], b"bcd");
        assert!(render_diagnostics().contains("reject="));
    }
}
