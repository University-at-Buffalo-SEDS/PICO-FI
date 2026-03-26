//! Upstream bridge - Now using I2C instead of SPI
//!
//! SPI slave mode on RP2040 is broken (known hardware issue since 2021)
//! Now using I2C0 on GPIO0 (SDA) and GPIO1 (SCL) for reliable communication

pub use crate::bridge::i2c::{UpstreamI2cDevice as UpstreamSpiDevice, I2cFrame, init_upstream_spi};
pub use crate::bridge::i2c::*;

// Re-export under "spi" names for compatibility
pub async fn run_client(
    uart: &mut embassy_rp::uart::BufferedUart,
    stack: embassy_net::Stack<'static>,
    host: [u8; 4],
    port: u16,
    _i2c: Option<&mut UpstreamSpiDevice>,
    _i2c_rx: embassy_sync::channel::Receiver<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, crate::bridge::spi_task::SpiFrame, 4>,
    _i2c_tx: embassy_sync::channel::Sender<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, crate::bridge::spi_task::SpiFrame, 4>,
    _bridge_config: crate::config::BridgeConfig,
    runtime: crate::bridge::runtime::BridgeRuntime<'_>,
) -> Result<(), ()> {
    // Use UART bridge instead for network connectivity
    crate::bridge::uart::run_client(uart, stack, host, port, _bridge_config, runtime).await
}

pub async fn run_server(
    uart: &mut embassy_rp::uart::BufferedUart,
    stack: embassy_net::Stack<'static>,
    port: u16,
    _i2c: Option<&mut UpstreamSpiDevice>,
    _i2c_rx: embassy_sync::channel::Receiver<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, crate::bridge::spi_task::SpiFrame, 4>,
    _i2c_tx: embassy_sync::channel::Sender<'static, embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, crate::bridge::spi_task::SpiFrame, 4>,
    _bridge_config: crate::config::BridgeConfig,
    runtime: crate::bridge::runtime::BridgeRuntime<'_>,
) -> Result<(), ()> {
    // Use UART bridge instead for network connectivity
    crate::bridge::uart::run_server(uart, stack, port, _bridge_config, runtime).await
}

