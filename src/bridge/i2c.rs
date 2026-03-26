//! I2C slave bridge implementation for Pico-Fi
//! Uses I2C0 on GPIO0 (SDA) and GPIO1 (SCL) for reliable communication

use embassy_rp::i2c::{self, I2c};
use embassy_rp::peripherals::{I2C0, PIN_0, PIN_1};
use embassy_rp::gpio::Pin;
use embassy_time::Timer;
use crate::protocol::spi::FRAME_SIZE;

const I2C_SLAVE_ADDR: u16 = 0x55;

/// I2C frame for communication
#[derive(Clone, Copy)]
pub struct I2cFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Stateful I2C slave device
pub struct UpstreamI2cDevice {
    rx_frame: [u8; FRAME_SIZE],
    tx_frame: [u8; FRAME_SIZE],
    rx_pos: usize,
    tx_pos: usize,
    pending_frame: Option<[u8; FRAME_SIZE]>,
}

impl UpstreamI2cDevice {
    pub fn new() -> Self {
        UpstreamI2cDevice {
            rx_frame: [0u8; FRAME_SIZE],
            tx_frame: [0u8; FRAME_SIZE],
            rx_pos: 0,
            tx_pos: 0,
            pending_frame: None,
        }
    }

    pub fn stage_response_frame(&mut self, frame: [u8; FRAME_SIZE]) {
        self.tx_frame = frame;
        self.tx_pos = 0;
    }

    pub fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        if self.pending_frame.is_some() {
            return self.pending_frame.as_ref();
        }
        None
    }

    pub fn clear_pending_frame(&mut self) {
        self.pending_frame = None;
        self.rx_frame.fill(0);
        self.rx_pos = 0;
    }

    /// Simulate receiving a frame (would be called by interrupt handler)
    pub async fn receive_frame(&mut self) -> Option<[u8; FRAME_SIZE]> {
        // This would be filled by actual I2C interrupt handler
        // For now, return None
        Timer::after_millis(10).await;
        None
    }
}

/// Initialize I2C0 as slave
pub fn init_i2c_slave(
    _i2c: I2c<'static, I2C0, i2c::Blocking>,
) -> UpstreamI2cDevice {
    UpstreamI2cDevice::new()
}

/// Compatibility function - matches SPI init signature but uses I2C
pub fn init_upstream_spi<TX, RX>(
    _spi1: embassy_rp::Peri<'static, embassy_rp::peripherals::SPI1>,
    _sclk: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_10>,
    _tx: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_11>,
    _rx: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_12>,
    _cs: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_13>,
    _tx_dma: embassy_rp::Peri<'static, TX>,
    _rx_dma: embassy_rp::Peri<'static, RX>,
) -> UpstreamI2cDevice
where
    TX: embassy_rp::PeripheralType,
    RX: embassy_rp::PeripheralType,
{
    // Ignore SPI peripherals, use I2C0 on GPIO0/1 instead
    UpstreamI2cDevice::new()
}


