//! Service UART receive while a buffered transmit is waiting for space.
use super::stream::FrameReader;
use embassy_futures::select::{Either, select};
use embedded_io_async::{Read, Write};

#[derive(Debug, PartialEq, Eq)]
pub enum UartStep {
    Written(usize),
    Received(u8, usize),
}

/// The writer must be cancellation safe (Embassy RP BufferedUartTx is): a
/// pending write has consumed no bytes. FrameReader retains partial RX state.
pub async fn uart_step(
    tx: &mut impl Write,
    chunk: &[u8],
    rx: &mut impl Read,
    reader: &mut FrameReader,
    payload: &mut [u8],
) -> Result<UartStep, ()> {
    match select(reader.read(rx, payload), tx.write(chunk)).await {
        Either::Second(Ok(written)) if written != 0 => Ok(UartStep::Written(written)),
        Either::First(Ok((magic, length))) => Ok(UartStep::Received(magic, length)),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        future::{Future, poll_fn},
        rc::Rc,
        task::{Context, Poll, Waker},
    };
    struct Tx {
        ready: Rc<Cell<usize>>,
        bytes: Vec<u8>,
    }
    struct Rx {
        available: Rc<Cell<usize>>,
        bytes: Vec<u8>,
        offset: usize,
    }
    impl embedded_io_async::ErrorType for Tx {
        type Error = core::convert::Infallible;
    }
    impl embedded_io_async::ErrorType for Rx {
        type Error = core::convert::Infallible;
    }
    impl Write for Tx {
        async fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            poll_fn(|_| {
                let n = bytes.len().min(self.ready.get());
                if n == 0 {
                    return Poll::Pending;
                }
                self.bytes.extend_from_slice(&bytes[..n]);
                self.ready.set(self.ready.get() - n);
                Poll::Ready(Ok(n))
            })
            .await
        }
        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    impl Read for Rx {
        async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
            poll_fn(|_| {
                let end = self.available.get().min(self.bytes.len());
                let n = out.len().min(end.saturating_sub(self.offset));
                if n == 0 {
                    return Poll::Pending;
                }
                out[..n].copy_from_slice(&self.bytes[self.offset..self.offset + n]);
                self.offset += n;
                Poll::Ready(Ok(n))
            })
            .await
        }
    }
    fn poll<F: Future>(future: F) -> Poll<F::Output> {
        let mut future = std::pin::pin!(future);
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }
    #[test]
    fn blocked_transmit_still_receives_and_partial_reads_survive_writes() {
        let ready = Rc::new(Cell::new(0));
        let available = Rc::new(Cell::new(0));
        let mut tx = Tx {
            ready: ready.clone(),
            bytes: vec![],
        };
        // More than the hardware's 512-byte RX buffer arrives while TX waits.
        let payload: Vec<u8> = (0..1024).map(|i| i as u8).collect();
        let mut rx = Rx {
            available: available.clone(),
            bytes: [vec![0xa5, 0x5a, 0, 4], payload.clone()].concat(),
            offset: 0,
        };
        let mut reader = FrameReader::new([[0xa5, 0x5a]; 2], false);
        let mut out = [0; 4096];
        available.set(400);
        assert_eq!(
            poll(uart_step(
                &mut tx,
                b"outbound",
                &mut rx,
                &mut reader,
                &mut out
            )),
            Poll::Pending
        );
        assert_eq!(rx.offset, 400, "RX must drain even though TX has no space");
        ready.set(3);
        assert_eq!(
            poll(uart_step(
                &mut tx,
                b"outbound",
                &mut rx,
                &mut reader,
                &mut out
            )),
            Poll::Ready(Ok(UartStep::Written(3)))
        );
        available.set(rx.bytes.len());
        assert_eq!(
            poll(uart_step(&mut tx, b"bound", &mut rx, &mut reader, &mut out)),
            Poll::Ready(Ok(UartStep::Received(0xa5, 1024)))
        );
        assert_eq!(&out[..1024], payload);
        ready.set(5);
        assert_eq!(
            poll(uart_step(&mut tx, b"bound", &mut rx, &mut reader, &mut out)),
            Poll::Ready(Ok(UartStep::Written(5)))
        );
        assert_eq!(
            tx.bytes, b"outbound",
            "partial TX progress is neither replayed nor skipped"
        );
    }
}
