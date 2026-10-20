//! Bounded cross-core frame storage. Never evict accepted stream frames.
use super::overwrite_queue::{OverwriteQueue, QueueItem};
use portable_atomic::{AtomicU32, Ordering};
pub const MAX_PAYLOAD: usize = 4096;
pub type Mailbox = OverwriteQueue<Packet, 128, 16384>;
pub static TO_NETWORK: Mailbox = Mailbox::new();
pub static TO_UART: Mailbox = Mailbox::new();
pub static UART_CORE_ID: AtomicU32 = AtomicU32::new(u32::MAX);
pub static NET_CORE_ID: AtomicU32 = AtomicU32::new(u32::MAX);
pub static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static RX_DROPPED: AtomicU32 = AtomicU32::new(0);
pub static TX_FRAMES: AtomicU32 = AtomicU32::new(0);
pub static RX_ERRORS: AtomicU32 = AtomicU32::new(0);
pub struct Packet {
    pub kind: u8,
    len: usize,
    data: [u8; MAX_PAYLOAD],
}
impl Packet {
    pub fn new(kind: u8) -> Self {
        Self {
            kind,
            len: 0,
            data: [0; MAX_PAYLOAD],
        }
    }
    pub fn buffer(&mut self) -> &mut [u8] {
        &mut self.data
    }
    pub fn finish(&mut self, kind: u8, len: usize) {
        self.kind = kind;
        self.len = len;
    }
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len]
    }
}
impl QueueItem for Packet {
    fn queued_len(&self) -> usize {
        self.len + 1
    }
    fn copy_queued_slice(&self, start: usize, out: &mut [u8]) {
        for (i, byte) in out.iter_mut().enumerate() {
            let p = start + i;
            *byte = if p == 0 { self.kind } else { self.data[p - 1] };
        }
    }
    fn from_queued_slices(first: &[u8], second: &[u8]) -> Option<Self> {
        let len = first.len() + second.len();
        if len == 0 || len > MAX_PAYLOAD + 1 {
            return None;
        }
        let mut bytes = first.iter().chain(second.iter());
        let mut packet = Self::new(*bytes.next()?);
        packet.len = len - 1;
        for (dst, src) in packet.data.iter_mut().zip(bytes) {
            *dst = *src;
        }
        Some(packet)
    }
}
/// UART has no hardware flow control. Drain it even if TCP stops, rejecting
/// whole NEW frames rather than overwriting older templates or partial frames.
pub fn admit_uart(packet: &Packet) {
    RX_FRAMES.fetch_add(1, Ordering::Relaxed);
    if !TO_NETWORK.try_push_ref(packet) {
        RX_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Frame both directions concurrently: awaiting TCP send capacity never
/// prevents TCP receive, and a full downlink queue never stops upload.
pub async fn forward_tcp(
    rx: &mut impl embedded_io_async::Read,
    tx: &mut impl embedded_io_async::Write,
    upload: &Mailbox,
    download: &Mailbox,
) -> Result<(), ()> {
    use embassy_futures::{
        select::{Either, select},
        yield_now,
    };
    let send = async {
        loop {
            let packet = upload.pop().await;
            write_frame(tx, [0xb5, 0x4e], packet.payload()).await?;
            yield_now().await;
        }
        #[allow(unreachable_code)]
        Ok::<(), ()>(())
    };
    let receive = async {
        let mut reader = crate::protocol::stream::FrameReader::new([[0xb5, 0x4e]; 2], false);
        loop {
            let mut packet = Packet::new(0xa5);
            let (_, n) = reader.read(rx, packet.buffer()).await?;
            packet.finish(0xa5, n);
            download.push(packet).await.map_err(|_| ())?;
            yield_now().await;
        }
        #[allow(unreachable_code)]
        Ok::<(), ()>(())
    };
    match select(send, receive).await {
        Either::First(r) | Either::Second(r) => r,
    }
}
/// One write for common small packets; no added waiting or batching delay.
pub async fn write_frame(
    tx: &mut impl embedded_io_async::Write,
    magic: [u8; 2],
    payload: &[u8],
) -> Result<(), ()> {
    let n = payload.len();
    let header = [magic[0], magic[1], n as u8, (n >> 8) as u8];
    if n <= 508 {
        let mut bytes = [0; 512];
        bytes[..4].copy_from_slice(&header);
        bytes[4..4 + n].copy_from_slice(payload);
        tx.write_all(&bytes[..4 + n]).await.map_err(|_| ())
    } else {
        tx.write_all(&header).await.map_err(|_| ())?;
        tx.write_all(payload).await.map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};
    fn packet(seq: u32, length: usize) -> Packet {
        let mut p = Packet::new(0xa5);
        p.buffer()[..4].copy_from_slice(&seq.to_le_bytes());
        for i in 4..length {
            p.buffer()[i] = (seq as usize + i) as u8;
        }
        p.finish(0xa5, length);
        p
    }
    fn assert_packet(p: &Packet, seq: u32, length: usize) {
        assert_eq!(p.kind, 0xa5);
        assert_eq!(p.payload().len(), length);
        assert_eq!(&p.payload()[..4], &seq.to_le_bytes());
        for i in 4..length {
            assert_eq!(p.payload()[i], (seq as usize + i) as u8);
        }
    }
    #[test]
    fn saturation_preserves_accepted_maximum_frames_and_reverse_traffic() {
        let upload = Mailbox::new();
        let download = Mailbox::new();
        for i in 0..3 {
            assert!(upload.try_push(packet(i, 4096)).is_ok());
        }
        // Four frames plus their kind bytes exceed the 16 KiB byte budget.
        assert!(upload.try_push(packet(3, 4096)).is_err());
        // A stalled network upload cannot block a command/ACK in reverse.
        assert!(download.try_push(packet(99, 64)).is_ok());
        assert_packet(&download.try_pop().unwrap(), 99, 64);
        for i in 0..3 {
            assert_packet(&upload.try_pop().unwrap(), i, 4096);
        }
        assert!(upload.try_pop().is_none());
    }
    #[test]
    fn cross_core_burst_wrap_and_backpressure_keep_exact_bytes_and_fifo() {
        let queue = Arc::new(Mailbox::new());
        let producer = queue.clone();
        let task = thread::spawn(move || {
            for seq in 0..20_000 {
                let len = if seq % 41 == 0 {
                    4096
                } else {
                    4 + seq as usize % 180
                };
                let mut p = packet(seq, len);
                loop {
                    match producer.try_push(p) {
                        Ok(()) => break,
                        Err(returned) => {
                            p = returned;
                            thread::yield_now();
                        }
                    }
                }
            }
        });
        for seq in 0..20_000 {
            let len = if seq % 41 == 0 {
                4096
            } else {
                4 + seq as usize % 180
            };
            loop {
                if let Some(p) = queue.try_pop() {
                    assert_packet(&p, seq, len);
                    break;
                }
                thread::yield_now();
            }
        }
        task.join().unwrap();
        assert!(queue.try_pop().is_none());
    }

    #[test]
    fn borrowed_enqueue_owns_the_bytes_before_returning() {
        let q = Mailbox::new();
        let mut p = packet(7, 64);
        assert!(q.try_push_ref(&p));
        p.buffer().fill(0);
        assert_packet(&q.try_pop().unwrap(), 7, 64);
    }
    struct Reader {
        data: std::vec::Vec<u8>,
        pos: usize,
    }
    impl embedded_io_async::ErrorType for Reader {
        type Error = core::convert::Infallible;
    }
    impl embedded_io_async::Read for Reader {
        async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
            core::future::poll_fn(|_| {
                let n = out.len().min(self.data.len() - self.pos);
                if n == 0 {
                    return core::task::Poll::Pending;
                }
                out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                core::task::Poll::Ready(Ok(n))
            })
            .await
        }
    }
    struct Writer {
        ready: std::rc::Rc<std::cell::Cell<bool>>,
        data: std::vec::Vec<u8>,
    }
    impl embedded_io_async::ErrorType for Writer {
        type Error = core::convert::Infallible;
    }
    impl embedded_io_async::Write for Writer {
        async fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            core::future::poll_fn(|_| {
                if !self.ready.get() {
                    return core::task::Poll::Pending;
                }
                let n = bytes.len().min(13); // Deliberately partial writes.
                self.data.extend_from_slice(&bytes[..n]);
                core::task::Poll::Ready(Ok(n))
            })
            .await
        }
        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    fn poll<F: core::future::Future>(f: core::pin::Pin<&mut F>) -> core::task::Poll<F::Output> {
        f.poll(&mut core::task::Context::from_waker(
            core::task::Waker::noop(),
        ))
    }
    fn wire(payload: &[u8]) -> std::vec::Vec<u8> {
        [
            std::vec![0xb5, 0x4e, payload.len() as u8, (payload.len() >> 8) as u8],
            payload.to_vec(),
        ]
        .concat()
    }
    #[test]
    fn blocked_tcp_upload_does_not_block_incoming_commands() {
        let upload = Mailbox::new();
        let download = Mailbox::new();
        assert!(upload.try_push(packet(1, 64)).is_ok());
        let mut rx = Reader {
            data: wire(b"command"),
            pos: 0,
        };
        let ready = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut tx = Writer {
            ready: ready.clone(),
            data: std::vec![],
        };
        {
            let f = forward_tcp(&mut rx, &mut tx, &upload, &download);
            let mut f = std::pin::pin!(f);
            assert!(poll(f.as_mut()).is_pending());
            assert_eq!(download.try_pop().unwrap().payload(), b"command");
            ready.set(true);
            for _ in 0..4 {
                assert!(poll(f.as_mut()).is_pending());
            }
        }
        assert_eq!(tx.data, wire(packet(1, 64).payload()));
    }
    #[test]
    fn blocked_command_queue_does_not_block_tcp_upload() {
        let upload = Mailbox::new();
        let download = Mailbox::new();
        for i in 0..3 {
            assert!(download.try_push(packet(i, 4096)).is_ok());
        }
        assert!(download.try_push(packet(4, 4096)).is_err());
        assert!(upload.try_push(packet(5, 1024)).is_ok());
        let mut rx = Reader {
            data: wire(packet(9, 4096).payload()),
            pos: 0,
        };
        let mut tx = Writer {
            ready: std::rc::Rc::new(std::cell::Cell::new(true)),
            data: std::vec![],
        };
        {
            let f = forward_tcp(&mut rx, &mut tx, &upload, &download);
            let mut f = std::pin::pin!(f);
            assert!(poll(f.as_mut()).is_pending());
            assert_packet(&download.try_pop().unwrap(), 0, 4096);
            assert!(poll(f.as_mut()).is_pending());
        }
        assert_eq!(tx.data, wire(packet(5, 1024).payload()));
        for i in [1, 2, 9] {
            assert_packet(&download.try_pop().unwrap(), i, 4096);
        }
    }
}
