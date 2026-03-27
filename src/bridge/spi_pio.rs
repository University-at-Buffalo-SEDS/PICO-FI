//! Starter pieces for a future PIO-backed SPI slave transport.
//!
//! This module is intentionally not wired into the active firmware path yet.
//! It collects the transaction/state handling that a PIO-driven SPI slave will
//! need, so the eventual state-machine service loop can focus on GPIO/PIO
//! mechanics instead of frame bookkeeping.
#![allow(dead_code)]

use crate::protocol::i2c::{FRAME_SIZE, RESP_DATA_MAGIC, make_response_frame};
use embedded_hal::spi::MODE_0;

/// Wire format required by the current host and firmware framing.
pub const SPI_PIO_FRAME_FORMAT: embedded_hal::spi::Mode = MODE_0;

/// Default pinout for the SPI upstream on RP2040 `SPI1`-compatible GPIOs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PioSpiPins {
    pub sck: u8,
    pub mosi: u8,
    pub miso: u8,
    pub cs: u8,
}

impl Default for PioSpiPins {
    fn default() -> Self {
        Self {
            sck: 10,
            mosi: 12,
            miso: 11,
            cs: 13,
        }
    }
}

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
    rx_frame: [u8; FRAME_SIZE],
    rx_pos: usize,
    rx_expected: usize,
    tx_pos: usize,
}

impl PioSpiTransportState {
    /// Creates a new state with the default empty data response staged.
    pub fn new() -> Self {
        Self {
            staged_tx: make_response_frame(RESP_DATA_MAGIC, b""),
            rx_frame: [0; FRAME_SIZE],
            rx_pos: 0,
            rx_expected: FRAME_SIZE,
            tx_pos: 0,
        }
    }

    /// Stages the next response frame for clock-out on the next transaction.
    pub fn stage_response(&mut self, frame: [u8; FRAME_SIZE]) {
        self.staged_tx = frame;
        self.tx_pos = 0;
    }

    /// Starts a new CS-bounded transaction.
    pub fn begin_transaction(&mut self) {
        self.rx_frame = [0; FRAME_SIZE];
        self.rx_pos = 0;
        self.rx_expected = FRAME_SIZE;
        self.tx_pos = 0;
    }

    /// Returns the next byte that should be presented on MISO.
    pub fn next_tx_byte(&mut self) -> u8 {
        let byte = if self.tx_pos < FRAME_SIZE {
            self.staged_tx[self.tx_pos]
        } else {
            0
        };
        if self.tx_pos < FRAME_SIZE {
            self.tx_pos += 1;
        }
        byte
    }

    /// Returns the next byte with a one-bit lead applied across the staged bitstream.
    ///
    /// This compensates for a slave TX path that presents each bit one clock late on MISO.
    pub fn next_tx_byte_shifted_left_1(&mut self) -> u8 {
        let byte = if self.tx_pos < FRAME_SIZE {
            let current = self.staged_tx[self.tx_pos];
            let next = if self.tx_pos + 1 < FRAME_SIZE {
                self.staged_tx[self.tx_pos + 1]
            } else {
                0
            };
            (current << 1) | (next >> 7)
        } else {
            0
        };
        if self.tx_pos < FRAME_SIZE {
            self.tx_pos += 1;
        }
        byte
    }

    /// Captures one received MOSI byte.
    pub fn capture_rx_byte(&mut self, byte: u8) {
        if self.rx_pos < FRAME_SIZE {
            self.rx_frame[self.rx_pos] = byte;
            self.rx_pos += 1;
            if self.rx_pos >= 2 {
                self.rx_expected = (self.rx_frame[1] as usize + 2).min(FRAME_SIZE);
            }
        }
    }

    /// Finalizes the current transaction and returns the captured result.
    pub fn finish_transaction(&mut self) -> TransactionResult {
        let tx_complete = self.tx_pos >= FRAME_SIZE;
        let received_any_nonzero = self.rx_frame[..self.rx_pos].iter().any(|&byte| byte != 0);
        let mut preview = [0u8; 8];
        let preview_len = self.rx_pos.min(preview.len());
        preview[..preview_len].copy_from_slice(&self.rx_frame[..preview_len]);
        let result = if self.rx_pos == 0 || !received_any_nonzero {
            TransactionResult::IdlePoll {
                received: self.rx_pos,
                preview,
            }
        } else if self.rx_complete() {
            TransactionResult::Complete(self.rx_frame)
        } else {
            TransactionResult::Partial {
                received: self.rx_pos,
                expected: self.rx_expected,
            }
        };

        if tx_complete {
            self.staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        }

        result
    }

    fn rx_complete(&self) -> bool {
        self.rx_pos >= 2 && self.rx_pos >= self.rx_expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::i2c::RESP_COMMAND_MAGIC;

    #[test]
    fn default_state_returns_idle_poll() {
        let mut state = PioSpiTransportState::new();
        state.begin_transaction();
        assert_eq!(
            state.finish_transaction(),
            TransactionResult::IdlePoll {
                received: 0,
                preview: [0; 8]
            }
        );
    }

    #[test]
    fn captures_complete_frame() {
        let mut state = PioSpiTransportState::new();
        state.begin_transaction();
        state.capture_rx_byte(0xa6);
        state.capture_rx_byte(0x04);
        state.capture_rx_byte(b'p');
        state.capture_rx_byte(b'i');
        state.capture_rx_byte(b'n');
        state.capture_rx_byte(b'g');
        match state.finish_transaction() {
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
        state.begin_transaction();
        state.capture_rx_byte(0xa6);
        state.capture_rx_byte(0x04);
        state.capture_rx_byte(b'p');
        assert_eq!(
            state.finish_transaction(),
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
        state.begin_transaction();
        assert_eq!(state.next_tx_byte(), RESP_COMMAND_MAGIC);
        assert_eq!(state.next_tx_byte(), 2);
    }
}
