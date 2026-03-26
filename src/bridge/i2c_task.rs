//! Dedicated I2C polling task built on the embassy-rp slave driver.

use crate::protocol::i2c::{FRAME_SIZE, RESP_DATA_MAGIC, make_response_frame};
use embassy_rp::i2c_slave::{Command, I2cSlave, ReadStatus};
use embassy_rp::peripherals::I2C0;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};

const I2C_CHUNK_MAX: usize = 32;

/// Message type for framed I2C transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct I2cFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the I2C bus, reassembling fixed 32-byte chunks into full frames.
pub async fn i2c_poll_task(
    i2c: &mut I2cSlave<'static, I2C0>,
    tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> ! {
    let mut transaction_buf = [0u8; I2C_CHUNK_MAX];
    let mut rx_frame = [0u8; FRAME_SIZE];
    let mut rx_pos = 0usize;
    let mut tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
    let mut tx_pos = 0usize;

    loop {
        if let Ok(resp) = rx_resp.try_receive() {
            tx_frame = resp.data;
            tx_pos = 0;
        }

        match i2c.listen(&mut transaction_buf).await {
            Ok(Command::Write(len)) => {
                append_chunk(&transaction_buf[..len], &mut rx_frame, &mut rx_pos);
                if rx_pos == FRAME_SIZE {
                    tx.send(I2cFrame { data: rx_frame }).await;
                    rx_frame = [0u8; FRAME_SIZE];
                    rx_pos = 0;
                }
            }
            Ok(Command::WriteRead(len)) => {
                append_chunk(&transaction_buf[..len], &mut rx_frame, &mut rx_pos);
                if rx_pos == FRAME_SIZE {
                    tx.send(I2cFrame { data: rx_frame }).await;
                    rx_frame = [0u8; FRAME_SIZE];
                    rx_pos = 0;
                }
                tx_pos = respond_chunk(i2c, &tx_frame, tx_pos).await;
            }
            Ok(Command::Read) => {
                tx_pos = respond_chunk(i2c, &tx_frame, tx_pos).await;
            }
            Ok(Command::GeneralCall(_)) => {}
            Err(_) => {
                i2c.reset();
                rx_frame = [0u8; FRAME_SIZE];
                rx_pos = 0;
                tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
                tx_pos = 0;
            }
        }
    }
}

fn append_chunk(chunk: &[u8], frame: &mut [u8; FRAME_SIZE], pos: &mut usize) {
    if *pos >= FRAME_SIZE || chunk.is_empty() {
        return;
    }

    let remaining = FRAME_SIZE.saturating_sub(*pos);
    let take = remaining.min(chunk.len());
    frame[*pos..*pos + take].copy_from_slice(&chunk[..take]);
    *pos += take;
}

async fn respond_chunk(
    i2c: &mut I2cSlave<'static, I2C0>,
    frame: &[u8; FRAME_SIZE],
    tx_pos: usize,
) -> usize {
    let remaining = FRAME_SIZE.saturating_sub(tx_pos);
    let send_len = remaining.min(I2C_CHUNK_MAX).max(1);
    let start = tx_pos.min(FRAME_SIZE.saturating_sub(1));
    let end = (start + send_len).min(FRAME_SIZE);

    match i2c.respond_and_fill(&frame[start..end], 0).await {
        Ok(ReadStatus::Done) | Ok(ReadStatus::LeftoverBytes(_)) => {
            if end >= FRAME_SIZE { 0 } else { end }
        }
        Ok(ReadStatus::NeedMoreBytes) => end.min(FRAME_SIZE),
        Err(_) => {
            i2c.reset();
            0
        }
    }
}
