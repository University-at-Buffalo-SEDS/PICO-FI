//! Frame progress survives cancellation of a pending read future.
use embedded_io_async::Read;

pub struct FrameReader {
    magic: [[u8; 2]; 2],
    resync: bool,
    header: [u8; 4],
    header_len: usize,
    payload_read: usize,
}

impl FrameReader {
    pub const fn new(magic: [[u8; 2]; 2], resync: bool) -> Self {
        Self {
            magic,
            resync,
            header: [0; 4],
            header_len: 0,
            payload_read: 0,
        }
    }

    pub fn reset(&mut self) {
        self.header_len = 0;
        self.payload_read = 0;
    }

    /// Retain this reader AND the same payload buffer across cancelled calls.
    /// The underlying single `read` must consume no bytes when returning Pending.
    pub async fn read(
        &mut self,
        input: &mut impl Read,
        payload: &mut [u8],
    ) -> Result<(u8, usize), ()> {
        while self.header_len < self.header.len() {
            let pos = self.header_len;
            // Commit progress immediately after each completed I/O operation.
            match input.read(&mut self.header[pos..pos + 1]).await {
                Ok(1) => self.header_len += 1,
                _ => {
                    self.reset();
                    return Err(());
                }
            }
            if self.header_len == 1 && !self.magic.iter().any(|m| m[0] == self.header[0]) {
                self.reset();
                if !self.resync {
                    return Err(());
                }
            } else if self.header_len == 2 && !self.magic.iter().any(|m| *m == self.header[..2]) {
                let last = self.header[1];
                self.reset();
                if !self.resync {
                    return Err(());
                }
                if self.magic.iter().any(|m| m[0] == last) {
                    self.header[0] = last;
                    self.header_len = 1;
                }
            }
        }
        let len = u16::from_le_bytes([self.header[2], self.header[3]]) as usize;
        if len > payload.len() {
            self.reset();
            return Err(());
        }
        while self.payload_read < len {
            match input.read(&mut payload[self.payload_read..len]).await {
                Ok(0) | Err(_) => {
                    self.reset();
                    return Err(());
                }
                Ok(n) => self.payload_read += n,
            }
        }
        let kind = self.header[0];
        self.reset();
        Ok((kind, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    use std::{cell::Cell, rc::Rc};

    struct Stream {
        bytes: Vec<u8>,
        pos: usize,
        available: Rc<Cell<usize>>,
        eof: bool,
    }
    impl embedded_io_async::ErrorType for Stream {
        type Error = core::convert::Infallible;
    }
    impl Read for Stream {
        async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
            core::future::poll_fn(|_| {
                if self.pos == self.bytes.len() && self.eof {
                    return Poll::Ready(Ok(0));
                }
                if self.pos >= self.available.get().min(self.bytes.len()) {
                    return Poll::Pending;
                }
                let n = out
                    .len()
                    .min(self.available.get().min(self.bytes.len()) - self.pos);
                out[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
                self.pos += n;
                Poll::Ready(Ok(n))
            })
            .await
        }
    }
    fn poll<F: Future>(f: F) -> Poll<F::Output> {
        let mut f = std::pin::pin!(f);
        f.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    }
    fn wire(magic: [u8; 2], payload: &[u8]) -> Vec<u8> {
        [
            magic.as_slice(),
            &(payload.len() as u16).to_le_bytes(),
            payload,
        ]
        .concat()
    }
    #[test]
    fn cancel_at_every_byte_boundary_and_resume_without_loss() {
        for magic in [[0xb5, 0x4e], [0xa5, 0x5a], [0xa6, 0x5b]] {
            let payload: Vec<_> = (0..251).collect();
            let bytes = wire(magic, &payload);
            for cut in 0..bytes.len() {
                let available = Rc::new(Cell::new(cut));
                let mut stream = Stream {
                    bytes: [bytes.clone(), wire(magic, b"next")].concat(),
                    pos: 0,
                    available: available.clone(),
                    eof: false,
                };
                let mut reader = FrameReader::new([magic, magic], false);
                let mut out = [0; 4096];
                assert_eq!(poll(reader.read(&mut stream, &mut out)), Poll::Pending);
                // Previous future was dropped, exactly as select drops its loser.
                available.set(stream.bytes.len());
                assert_eq!(
                    poll(reader.read(&mut stream, &mut out)),
                    Poll::Ready(Ok((magic[0], payload.len())))
                );
                assert_eq!(&out[..payload.len()], payload.as_slice());
                assert_eq!(
                    poll(reader.read(&mut stream, &mut out)),
                    Poll::Ready(Ok((magic[0], 4)))
                );
                assert_eq!(&out[..4], b"next");
            }
        }
    }
    #[test]
    fn repeated_cancellation_in_both_directions_preserves_progress() {
        let mut readers = [
            FrameReader::new([[0xb5, 0x4e]; 2], false),
            FrameReader::new([[0xa5, 0x5a]; 2], true),
        ];
        let mut streams = [
            Stream {
                bytes: wire([0xb5, 0x4e], b"to uart"),
                pos: 0,
                available: Rc::new(Cell::new(0)),
                eof: false,
            },
            Stream {
                bytes: wire([0xa5, 0x5a], b"to tcp!"),
                pos: 0,
                available: Rc::new(Cell::new(0)),
                eof: false,
            },
        ];
        let mut output = [[0; 32]; 2];
        for i in 0..11 {
            for side in 0..2 {
                streams[side].available.set(i);
                assert_eq!(
                    poll(readers[side].read(&mut streams[side], &mut output[side])),
                    Poll::Pending
                );
            }
        }
        for side in 0..2 {
            streams[side].available.set(11);
            assert!(matches!(
                poll(readers[side].read(&mut streams[side], &mut output[side])),
                Poll::Ready(Ok((_, 7)))
            ));
        }
        assert_eq!(&output[0][..7], b"to uart");
        assert_eq!(&output[1][..7], b"to tcp!");
    }
    #[test]
    fn empty_frames_resync_oversize_and_truncated_stream() {
        let mut reader = FrameReader::new([[0xa5, 0x5a], [0xa6, 0x5b]], true);
        let mut stream = Stream {
            bytes: [vec![1, 0xa5], wire([0xa6, 0x5b], b"")].concat(),
            pos: 0,
            available: Rc::new(Cell::new(999)),
            eof: true,
        };
        let mut out = [0; 8];
        assert_eq!(
            poll(reader.read(&mut stream, &mut out)),
            Poll::Ready(Ok((0xa6, 0)))
        );
        stream.bytes = wire([0xa5, 0x5a], b"too large");
        stream.pos = 0;
        assert_eq!(
            poll(reader.read(&mut stream, &mut out)),
            Poll::Ready(Err(()))
        );
        stream.bytes = vec![0xa5, 0x5a, 4, 0, 1];
        stream.pos = 0;
        assert_eq!(
            poll(reader.read(&mut stream, &mut out)),
            Poll::Ready(Err(()))
        );
        stream.bytes = wire([0xa5, 0x5a], b"ok");
        stream.pos = 0;
        assert_eq!(
            poll(reader.read(&mut stream, &mut out)),
            Poll::Ready(Ok((0xa5, 2)))
        );
    }
}
