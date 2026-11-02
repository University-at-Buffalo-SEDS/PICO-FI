//! Wire-level protocol definitions used by bridge transports.

pub mod i2c;

pub mod stream;

pub mod discovery;
pub mod ipv4ll;

pub mod auto_address;

pub mod dhcp_probe;

#[cfg(pico_uart_sedsnet_crc32)]
pub mod strict_uart;
