//! Dedicated I2C polling task built on the embassy-rp slave driver.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_futures::select::{Either, select};
use embassy_rp::i2c_slave::{Command, I2cSlave, ReadStatus};
use embassy_rp::peripherals::I2C0;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::Timer;
use portable_atomic::AtomicBool;

const I2C_CHUNK_MAX: usize = 32;
const RESPONSE_WAIT_MS: u64 = 20;

/// Message type for framed I2C transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct I2cFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the I2C bus, reassembling fixed 32-byte chunks into full frames.
pub async fn i2c_poll_task(
    i2c: &mut I2cSlave<'static, I2C0>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> ! {
    let mut transaction_buf = [0u8; I2C_CHUNK_MAX];
    let mut rx_frame = [0u8; FRAME_SIZE];
    let mut rx_pos = 0usize;
    let mut rx_expected = FRAME_SIZE;
    let mut tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
    let mut tx_pos = 0usize;

    loop {
        if let Ok(resp) = rx_resp.try_receive() {
            tx_frame = resp.data;
            tx_pos = 0;
        }

        match i2c.listen(&mut transaction_buf).await {
            Ok(Command::Write(len)) => {
                append_chunk(
                    &transaction_buf[..len],
                    &mut rx_frame,
                    &mut rx_pos,
                    &mut rx_expected,
                );
                if rx_complete(rx_pos, rx_expected) {
                    process_complete_frame(
                        rx_frame,
                        bridge_config,
                        link_active,
                        &mut tx_frame,
                        &mut tx_pos,
                        tx,
                    )
                    .await;
                    reset_rx_state(&mut rx_frame, &mut rx_pos, &mut rx_expected);
                }
            }
            Ok(Command::WriteRead(len)) => {
                append_chunk(
                    &transaction_buf[..len],
                    &mut rx_frame,
                    &mut rx_pos,
                    &mut rx_expected,
                );
                if rx_complete(rx_pos, rx_expected) {
                    process_complete_frame(
                        rx_frame,
                        bridge_config,
                        link_active,
                        &mut tx_frame,
                        &mut tx_pos,
                        tx,
                    )
                    .await;
                    reset_rx_state(&mut rx_frame, &mut rx_pos, &mut rx_expected);
                }
                await_response_frame(&rx_resp, &mut tx_frame, &mut tx_pos).await;
                tx_pos = respond_chunk(i2c, &mut tx_frame, tx_pos).await;
            }
            Ok(Command::Read) => {
                await_response_frame(&rx_resp, &mut tx_frame, &mut tx_pos).await;
                tx_pos = respond_chunk(i2c, &mut tx_frame, tx_pos).await;
            }
            Ok(Command::GeneralCall(_)) => {}
            Err(_) => {
                i2c.reset();
                reset_rx_state(&mut rx_frame, &mut rx_pos, &mut rx_expected);
                tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
                tx_pos = 0;
            }
        }
    }
}

async fn process_complete_frame(
    frame: [u8; FRAME_SIZE],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx_frame: &mut [u8; FRAME_SIZE],
    tx_pos: &mut usize,
    tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) {
    if let Some(response) = handle_local_request(frame, bridge_config, link_active) {
        *tx_frame = response;
        *tx_pos = 0;
    } else {
        tx.send(I2cFrame { data: frame }).await;
    }
}

fn reset_rx_state(
    rx_frame: &mut [u8; FRAME_SIZE],
    rx_pos: &mut usize,
    rx_expected: &mut usize,
) {
    *rx_frame = [0u8; FRAME_SIZE];
    *rx_pos = 0;
    *rx_expected = FRAME_SIZE;
}

fn rx_complete(rx_pos: usize, rx_expected: usize) -> bool {
    rx_pos >= 2 && rx_pos >= rx_expected
}

fn handle_local_request(
    frame: [u8; FRAME_SIZE],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Option<[u8; FRAME_SIZE]> {
    match parse_request_frame(&frame) {
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response = render_local_bridge_command(bridge_config, link_active, line);
            Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()))
        }
        _ => None,
    }
}

async fn await_response_frame(
    rx_resp: &Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    tx_frame: &mut [u8; FRAME_SIZE],
    tx_pos: &mut usize,
) {
    if *tx_pos != 0 {
        return;
    }

    if let Ok(resp) = rx_resp.try_receive() {
        *tx_frame = resp.data;
        *tx_pos = 0;
        return;
    }

    match select(rx_resp.receive(), Timer::after_millis(RESPONSE_WAIT_MS)).await {
        Either::First(resp) => {
            *tx_frame = resp.data;
            *tx_pos = 0;
        }
        Either::Second(_) => {}
    }
}

fn append_chunk(
    chunk: &[u8],
    frame: &mut [u8; FRAME_SIZE],
    pos: &mut usize,
    expected: &mut usize,
) {
    if *pos >= FRAME_SIZE || chunk.is_empty() {
        return;
    }

    let remaining = FRAME_SIZE.saturating_sub(*pos);
    let take = remaining.min(chunk.len());
    frame[*pos..*pos + take].copy_from_slice(&chunk[..take]);
    *pos += take;

    if *pos >= 2 {
        let payload_len = frame[1] as usize;
        *expected = (payload_len + 2).min(FRAME_SIZE);
    }
}

async fn respond_chunk(
    i2c: &mut I2cSlave<'static, I2C0>,
    frame: &mut [u8; FRAME_SIZE],
    tx_pos: usize,
) -> usize {
    let remaining = FRAME_SIZE.saturating_sub(tx_pos);
    let send_len = remaining.min(I2C_CHUNK_MAX).max(1);
    let start = tx_pos.min(FRAME_SIZE.saturating_sub(1));
    let end = (start + send_len).min(FRAME_SIZE);

    match i2c.respond_and_fill(&frame[start..end], 0).await {
        Ok(ReadStatus::Done) | Ok(ReadStatus::LeftoverBytes(_)) => {
            if end >= FRAME_SIZE {
                *frame = make_response_frame(RESP_DATA_MAGIC, b"");
                0
            } else {
                end
            }
        }
        Ok(ReadStatus::NeedMoreBytes) => end.min(FRAME_SIZE),
        Err(_) => {
            i2c.reset();
            *frame = make_response_frame(RESP_DATA_MAGIC, b"");
            0
        }
    }
}
