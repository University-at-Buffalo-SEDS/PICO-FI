//! Dedicated I2C polling task for reliable transaction handling.

use crate::bridge::i2c::UpstreamI2cDevice;
use crate::protocol::i2c::FRAME_SIZE;
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};

/// Message type for framed I2C transfers passed between the poller and bridge session.
#[derive(Clone, Copy)]
pub struct I2cFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously polls the I2C peripheral and forwards completed frames to the bridge.
pub async fn i2c_poll_task(
    i2c: &mut UpstreamI2cDevice,
    tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> ! {
    loop {
        if let Ok(resp) = rx_resp.try_receive() {
            i2c.stage_response_frame(resp.data);
        }

        if let Some(frame) = i2c.poll_transaction() {
            let _ = tx.try_send(I2cFrame { data: *frame });
            i2c.clear_pending_frame();
        }

        yield_now().await;
    }
}
