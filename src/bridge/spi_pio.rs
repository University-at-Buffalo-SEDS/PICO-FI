//! Framing helpers for the upstream PIO-backed SPI slave transport.

use crate::protocol::i2c::{
    FRAME_SIZE, REQ_COMMAND_MAGIC, REQ_DATA_MAGIC, RESP_DATA_MAGIC, make_response_frame,
};
/// Result of one CS-bounded SPI transaction as seen by the future PIO backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionResult {
    IdlePoll { received: usize, preview: [u8; 8] },
    Partial {
        received: usize,
        expected: usize,
        frame: [u8; FRAME_SIZE],
    },
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
        let (rx_frame, received) = normalize_request_frame(rx_frame, received);
        let scan_len = received.max(8).min(FRAME_SIZE);
        let received_any_nonzero = rx_frame.iter().take(scan_len).any(|&byte| byte != 0);
        let mut preview = [0u8; 8];
        let preview_len = scan_len.min(preview.len());
        preview[..preview_len].copy_from_slice(&rx_frame[..preview_len]);
        let expected = if received >= 2 || (rx_frame[0] != 0 || rx_frame[1] != 0) {
            (rx_frame[1] as usize + 2).min(FRAME_SIZE)
        } else {
            FRAME_SIZE
        };
        let result = if !received_any_nonzero {
            TransactionResult::IdlePoll { received, preview }
        } else if expected <= FRAME_SIZE && scan_len.max(received) >= expected {
            TransactionResult::Complete(rx_frame)
        } else {
            TransactionResult::Partial {
                received,
                expected,
                frame: rx_frame,
            }
        };

        self.staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        result
    }
}

fn normalize_request_frame(
    rx_frame: &[u8; FRAME_SIZE],
    received: usize,
) -> ([u8; FRAME_SIZE], usize) {
    let received = received.min(FRAME_SIZE);
    let scan_len = received.max(8).min(FRAME_SIZE);
    if matches!(rx_frame[0], REQ_DATA_MAGIC | REQ_COMMAND_MAGIC) {
        return (*rx_frame, received);
    }

    for offset in 1..scan_len {
        if !matches!(rx_frame[offset], REQ_DATA_MAGIC | REQ_COMMAND_MAGIC) {
            continue;
        }

        let mut normalized = [0u8; FRAME_SIZE];
        let copied = FRAME_SIZE - offset;
        normalized[..copied].copy_from_slice(&rx_frame[offset..]);

        let adjusted_received = received
            .saturating_sub(offset)
            .max(scan_len.saturating_sub(offset))
            .min(FRAME_SIZE);
        return (normalized, adjusted_received);
    }

    (*rx_frame, received)
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
                expected: 6,
                frame
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

    #[test]
    fn normalizes_shifted_command_frame() {
        let mut state = PioSpiTransportState::new();
        let mut frame = [0u8; FRAME_SIZE];
        frame[1..7].copy_from_slice(&[0xa6, 0x04, b'p', b'i', b'n', b'g']);
        match state.finish_transaction(&frame, 7) {
            TransactionResult::Complete(frame) => {
                assert_eq!(frame[0], 0xa6);
                assert_eq!(frame[1], 0x04);
                assert_eq!(&frame[2..6], b"ping");
            }
            other => panic!("expected normalized complete frame, got {other:?}"),
        }
    }

    #[test]
    fn normalizes_duplicate_length_command_frame_with_bad_count() {
        let mut state = PioSpiTransportState::new();
        let mut frame = [0u8; FRAME_SIZE];
        frame[..17].copy_from_slice(&[
            0x0f, 0x0f, b'[', b'1', b'0', b'.', b'8', b'.', b'0', b'.', b'6', b']', b' ',
            b'h', b'e', b'y', b'\n',
        ]);
        assert!(matches!(
            state.finish_transaction(&frame, 0),
            TransactionResult::Partial { .. }
        ));
    }

    #[test]
    fn normalizes_duplicate_length_slash_command_with_bad_count() {
        let mut state = PioSpiTransportState::new();
        let mut frame = [0u8; FRAME_SIZE];
        frame[..8].copy_from_slice(&[0x06, 0x06, b'/', b'l', b'i', b'n', b'k', b'\n']);
        assert!(matches!(
            state.finish_transaction(&frame, 1),
            TransactionResult::Partial { .. }
        ));
    }
}
