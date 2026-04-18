//! Shared frame type for framed SPI transfers.

use crate::bridge::overwrite_queue::QueueItem;
use crate::protocol::i2c::{FRAME_HEADER_SIZE, FRAME_SIZE, build_frame_into};
use heapless::Vec;

/// Message type for framed SPI transfers passed between the bus task and bridge session.
pub struct SpiFrame {
    data: Vec<u8, FRAME_SIZE>,
}

impl SpiFrame {
    pub fn from_raw_frame(data: [u8; FRAME_SIZE]) -> Self {
        let payload_len = u16::from_le_bytes([data[2], data[3]]) as usize;
        let len = FRAME_HEADER_SIZE
            .saturating_add(payload_len)
            .min(FRAME_SIZE);
        let mut frame = Vec::new();
        let _ = frame.extend_from_slice(&data[..len]);
        Self { data: frame }
    }

    pub fn response(magic: u8, payload: &[u8]) -> Self {
        let mut data = [0u8; FRAME_SIZE];
        let len = build_frame_into(magic, payload, &mut data).unwrap_or(0);
        let mut frame = Vec::new();
        let _ = frame.extend_from_slice(&data[..len]);
        Self { data: frame }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    pub fn as_frame(&self) -> [u8; FRAME_SIZE] {
        let mut frame = [0u8; FRAME_SIZE];
        frame[..self.data.len()].copy_from_slice(self.data.as_slice());
        frame
    }
}

impl QueueItem for SpiFrame {
    fn queued_len(&self) -> usize {
        self.data.len()
    }
}
