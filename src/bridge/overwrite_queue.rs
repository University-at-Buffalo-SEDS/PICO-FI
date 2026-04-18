//! Small overwrite-on-full buffers for lossy bridge egress paths.

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::Deque;

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
        if item_len > MAX_BYTES {
            return false;
        }

        let pushed = unsafe {
            self.state.lock_mut(|state| {
                while state.queue.is_full()
                    || state.queued_bytes.saturating_add(item_len) > MAX_BYTES
                {
                    match state.queue.pop_front() {
                        Some(dropped) => {
                            state.queued_bytes =
                                state.queued_bytes.saturating_sub(dropped.queued_len());
                        }
                        None => break,
                    }
                }

                if state.queue.push_back(item).is_ok() {
                    state.queued_bytes = state.queued_bytes.saturating_add(item_len);
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
        let packet = BytePacket::from_slices(slices);
        if packet.len == 0 {
            return;
        }

        if self.queue.is_full() {
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
        self.queue.clear();
        self.front_offset = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
