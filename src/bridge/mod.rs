//! Bridge implementations for each upstream role.

pub mod commands;
pub mod i2c;
pub mod i2c_task;
pub mod overwrite_queue;
pub mod runtime;
pub mod spi;
pub mod spi_diag;
pub mod spi_frame;
pub mod spi_hw_task;
pub mod spi_pio;
pub mod test;
pub mod usb;
pub mod uart;
