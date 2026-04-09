//! Framing helpers for the upstream PIO-backed SPI slave transport.

use crate::protocol::i2c::{FRAME_SIZE, RESP_DATA_MAGIC, make_response_frame};
/// Result of one CS-bounded SPI transaction as seen by the future PIO backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionResult {
    IdlePoll { received: usize, preview: [u8; 8] },
    Partial { received: usize, expected: usize },
    Complete([u8; FRAME_SIZE]),
}

/// Frame staging and per-transaction bookkeeping for a PIO SPI slave.
#[derive(Clone, Copy)]
pub struct PioSpiTransportState {
    staged_tx: [u8; FRAME_SIZE],
}

impl PioSpiTransportState {
    /// Creates a new state with the default empty data response staged.
    pub fn new() -> Self {
        Self {
            staged_tx: make_response_frame(RESP_DATA_MAGIC, b""),
        }
    }

    /// Stages the next response frame for clock-out on the next transaction.
    pub fn stage_response(&mut self, frame: [u8; FRAME_SIZE]) {
        self.staged_tx = frame;
    }

    /// Returns the currently staged response frame.
    pub fn staged_response(&self) -> [u8; FRAME_SIZE] {
        self.staged_tx
    }

    /// Finalizes a CS-bounded transaction from the DMA receive buffer.
    pub fn finish_transaction(
        &mut self,
        rx_frame: &[u8; FRAME_SIZE],
        received: usize,
    ) -> TransactionResult {
        let received = received.min(FRAME_SIZE);
        let received_any_nonzero = rx_frame.iter().take(received.max(8)).any(|&byte| byte != 0);
        let mut preview = [0u8; 8];
        let preview_len = received.max(preview.len()).min(FRAME_SIZE).min(preview.len());
        preview[..preview_len].copy_from_slice(&rx_frame[..preview_len]);
        let expected = if received >= 2 || (rx_frame[0] != 0 || rx_frame[1] != 0) {
            (rx_frame[1] as usize + 2).min(FRAME_SIZE)
        } else {
            FRAME_SIZE
        };
        let result = if !received_any_nonzero {
            TransactionResult::IdlePoll { received, preview }
        } else if expected <= FRAME_SIZE && received.max(8) >= expected {
            TransactionResult::Complete(*rx_frame)
        } else {
            TransactionResult::Partial { received, expected }
        };

        self.staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::i2c::RESP_COMMAND_MAGIC;

    #[test]
    fn default_state_returns_idle_poll() {
        let mut state = PioSpiTransportState::new();
        assert_eq!(
            state.finish_transaction(&[0; FRAME_SIZE], 0),
            TransactionResult::IdlePoll {
                received: 0,
                preview: [0; 8]
            }
        );
    }

    #[test]
    fn captures_complete_frame() {
        let mut state = PioSpiTransportState::new();
        let mut frame = [0u8; FRAME_SIZE];
        frame[..6].copy_from_slice(&[0xa6, 0x04, b'p', b'i', b'n', b'g']);
        match state.finish_transaction(&frame, 6) {
            TransactionResult::Complete(frame) => {
                assert_eq!(frame[0], 0xa6);
                assert_eq!(frame[1], 0x04);
            }
            other => panic!("expected complete frame, got {other:?}"),
        }
    }

    #[test]
    fn captures_partial_frame() {
        let mut state = PioSpiTransportState::new();
        let mut frame = [0u8; FRAME_SIZE];
        frame[..3].copy_from_slice(&[0xa6, 0x04, b'p']);
        assert_eq!(
            state.finish_transaction(&frame, 3),
            TransactionResult::Partial {
                received: 3,
                expected: 6
            }
        );
    }

    #[test]
    fn drains_staged_response_once() {
        let mut state = PioSpiTransportState::new();
        state.stage_response(make_response_frame(RESP_COMMAND_MAGIC, b"ok"));
        let frame = state.staged_response();
        assert_eq!(frame[0], RESP_COMMAND_MAGIC);
        assert_eq!(frame[1], 2);
        let _ = state.finish_transaction(&[0; FRAME_SIZE], FRAME_SIZE);
        assert_eq!(
            state.staged_response(),
            make_response_frame(RESP_DATA_MAGIC, b"")
        );
    }
}
