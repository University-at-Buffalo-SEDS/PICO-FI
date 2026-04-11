//! Shared frame type for framed SPI transfers.

use crate::protocol::i2c::FRAME_SIZE;

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}
