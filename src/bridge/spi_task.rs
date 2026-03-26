//! Core 1 dedicated SPI polling task for reliable transaction handling.

use crate::bridge::spi::UpstreamSpiDevice;
use crate::protocol::spi::FRAME_SIZE;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_futures::yield_now;

/// Message type for SPI frames passed from core 1 to core 0
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Dedicated SPI polling task running on core 1
/// 
/// This task continuously polls the SPI peripheral for completed transactions
/// and sends any received frames to the core 0 bridge via a channel.
/// By dedicating core 1 to SPI polling, we ensure the SPI transactions
/// are never starved by network I/O operations on core 0.
///
/// It also receives response frames from core 0 via a separate channel and stages
/// them for transmission on the next SPI transaction.
pub async fn spi_poll_task(
    spi: &mut UpstreamSpiDevice,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> ! {
    loop {
        // Process any pending response frames from core 0
        if let Ok(resp) = rx_resp.try_receive() {
            spi.stage_response_frame(resp.data);
        }

        // Poll for completed SPI transactions
        if let Some(frame) = spi.poll_transaction() {
            let msg = SpiFrame {
                data: *frame,
            };
            // Send to core 0 (non-blocking with bounded buffer)
            let _ = tx.try_send(msg);
            spi.clear_pending_frame();
        }
        yield_now().await;
    }
}





