//! Hardware-SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, signal_led_activity, trim_ascii_line};
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    make_response_frame, parse_request_frame, RequestFrame, FRAME_SIZE, REQ_COMMAND_MAGIC,
    REQ_DATA_MAGIC, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC,
};
use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use heapless::String;
use portable_atomic::AtomicBool;

const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(5);
const SPI_CS_PIN: usize = 13;
const SPI_SCK_PIN: usize = 10;
const SPI_MOSI_PIN: usize = 12;

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the SPI1 slave bus and bridges framed requests.
#[allow(clippy::too_many_arguments)]
pub async fn spi_poll_task(
    _pio1: Peri<'static, PIO1>,
    _sclk: Peri<'static, PIN_10>,
    _miso: Peri<'static, PIN_11>,
    _mosi: Peri<'static, PIN_12>,
    _cs: Peri<'static, PIN_13>,
    _tx_dma: Peri<'static, DMA_CH2>,
    _rx_dma: Peri<'static, DMA_CH3>,
    _spawner: Spawner,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> ! {
    let _pio1 = _pio1;
    let _tx_dma = _tx_dma;
    let _rx_dma = _rx_dma;
    let _spawner = _spawner;

    let _sclk = Input::new(_sclk, Pull::None);
    let mut miso = Output::new(_miso, Level::Low);
    let _mosi = Input::new(_mosi, Pull::None);
    let _cs = Input::new(_cs, Pull::None);

    let echo_mode = matches!(
        bridge_config.upstream_mode,
        crate::config::UpstreamMode::SpiEcho
    );
    let static_mode = matches!(
        bridge_config.upstream_mode,
        crate::config::UpstreamMode::SpiStatic
    );
    let static_frame = make_response_frame(RESP_COMMAND_MAGIC, b"pong");
    let mut transport = PioSpiTransportState::new();

    loop {
        if static_mode {
            transport.stage_response(static_frame);
        } else if !echo_mode {
            if let Ok(resp) = rx_resp.try_receive() {
                transport.stage_response(resp.data);
            }
        }

        while !spi_cs_asserted() {
            if static_mode {
                transport.stage_response(static_frame);
            } else if !echo_mode {
                if let Ok(resp) = rx_resp.try_receive() {
                    transport.stage_response(resp.data);
                }
            }
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        let result = software_spi_transaction(&mut miso, &mut transport);

        if static_mode {
            continue;
        }

        if echo_mode {
            match result {
                TransactionResult::Complete(frame) => transport.stage_response(frame),
                TransactionResult::IdlePoll { .. } => transport.stage_response(make_response_frame(RESP_DATA_MAGIC, b"")),
                TransactionResult::Partial { .. } => {
                    transport.stage_response(make_response_frame(RESP_COMMAND_MAGIC, b"error partial spi frame"))
                }
            }
            continue;
        }

        if let Some(next) = finalize_transaction(result, bridge_config, link_active, tx).await {
            transport.stage_response(next);
        }
    }
}

fn software_spi_transaction(
    miso: &mut Output<'static>,
    transport: &mut PioSpiTransportState,
) -> TransactionResult {
    transport.begin_transaction();
    while spi_cs_asserted() && spi_sclk_high() {
        core::hint::spin_loop();
    }

    let mut tx_byte = transport.next_tx_byte();
    let mut tx_mask = 0x80u8;
    drive_miso(miso, tx_byte & tx_mask != 0);

    let mut rx_byte = 0u8;
    let mut rx_bits = 0u8;

    while spi_cs_asserted() {
        while spi_cs_asserted() && !spi_sclk_high() {
            core::hint::spin_loop();
        }
        if !spi_cs_asserted() {
            break;
        }

        rx_byte = (rx_byte << 1) | (spi_mosi_high() as u8);
        rx_bits += 1;

        while spi_cs_asserted() && spi_sclk_high() {
            core::hint::spin_loop();
        }
        if !spi_cs_asserted() {
            break;
        }

        if rx_bits == 8 {
            transport.capture_rx_byte(rx_byte);
            rx_byte = 0;
            rx_bits = 0;
        }

        if tx_mask == 0x01 {
            tx_byte = transport.next_tx_byte();
            tx_mask = 0x80;
        } else {
            tx_mask >>= 1;
        }
        drive_miso(miso, tx_byte & tx_mask != 0);
    }

    drive_miso(miso, false);
    transport.finish_transaction()
}

async fn finalize_transaction(
    result: TransactionResult,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> Option<[u8; FRAME_SIZE]> {
    let frame = match result {
        TransactionResult::IdlePoll { received, preview } => {
            if received == 0 {
                return Some(make_response_frame(RESP_DATA_MAGIC, b""));
            }
            return Some(make_response_frame(
                RESP_COMMAND_MAGIC,
                render_spi_idle_capture(received, &preview).as_bytes(),
            ));
        }
        TransactionResult::Partial { .. } => {
            return Some(make_response_frame(RESP_COMMAND_MAGIC, b"error partial spi frame"));
        }
        TransactionResult::Complete(frame) => frame,
    };

    if let Some(payload) = extract_local_command_payload(&frame) {
        signal_led_activity();
        let line = trim_ascii_line(payload);
        let response = render_local_bridge_command(bridge_config, link_active, line);
        return Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()));
    }

    match parse_request_frame(&frame) {
        Some(RequestFrame::Command(_payload)) => {
            unreachable!("handled by extract_local_command_payload")
        }
        Some(RequestFrame::Data(_)) => {
            tx.send(SpiFrame { data: frame }).await;
            None
        }
        None => Some(make_response_frame(RESP_COMMAND_MAGIC, b"error invalid spi frame")),
    }
}

fn extract_local_command_payload(frame: &[u8; FRAME_SIZE]) -> Option<&[u8]> {
    match parse_request_frame(frame) {
        Some(RequestFrame::Command(payload)) => {
            if is_plausible_local_command(payload) {
                return Some(payload);
            }
        }
        Some(RequestFrame::Data(payload)) => {
            if is_plausible_local_command(payload) {
                return Some(payload);
            }
        }
        None => {}
    }

    recover_spi_command_payload(frame).filter(|payload| is_plausible_local_command(payload))
}

fn is_plausible_local_command(payload: &[u8]) -> bool {
    payload.first() == Some(&b'/')
        && payload
            .iter()
            .all(|&byte| byte == b'\n' || byte == b'\r' || (32..=126).contains(&byte))
}

fn render_spi_idle_capture(received: usize, preview: &[u8; 8]) -> String<96> {
    let mut out = String::<96>::new();
    let _ = out.push_str("rx=");
    push_usize(&mut out, received);
    let _ = out.push_str(" b=");
    for (index, byte) in preview.iter().take(4).enumerate() {
        if index != 0 {
            let _ = out.push('-');
        }
        push_hex(&mut out, *byte);
    }
    out
}

fn push_hex(out: &mut String<96>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}

fn push_usize(out: &mut String<96>, mut value: usize) {
    if value == 0 {
        let _ = out.push('0');
        return;
    }

    let mut digits = [0u8; 20];
    let mut len = 0usize;
    while value > 0 {
        digits[len] = (value % 10) as u8;
        len += 1;
        value /= 10;
    }

    while len > 0 {
        len -= 1;
        let _ = out.push((b'0' + digits[len]) as char);
    }
}

fn recover_spi_command_payload(frame: &[u8; FRAME_SIZE]) -> Option<&[u8]> {
    let header_looks_corrupt = matches!(
        (frame[0], frame[1]),
        (REQ_DATA_MAGIC, 0) | (REQ_COMMAND_MAGIC, 0) | (0, 0)
    );
    if !header_looks_corrupt {
        return None;
    }

    let slash_index = frame[2..]
        .iter()
        .position(|&byte| byte == b'/')
        .map(|index| index + 2)?;
    let end = frame[slash_index..]
        .iter()
        .position(|&byte| byte == 0)
        .map(|index| slash_index + index)
        .unwrap_or(FRAME_SIZE);
    let payload = &frame[slash_index..end];
    if payload.is_empty() {
        return None;
    }

    let plausible = payload
        .iter()
        .all(|&byte| byte == b'\n' || byte == b'\r' || (32..=126).contains(&byte));
    if plausible { Some(payload) } else { None }
}


fn spi_cs_asserted() -> bool {
    !read_pin(SPI_CS_PIN)
}

fn spi_sclk_high() -> bool {
    read_pin(SPI_SCK_PIN)
}

fn spi_mosi_high() -> bool {
    read_pin(SPI_MOSI_PIN)
}

fn read_pin(pin: usize) -> bool {
    let bank0_inputs = rp_pac::SIO.gpio_in(0).read();
    ((bank0_inputs >> pin) & 1) != 0
}

fn drive_miso(miso: &mut Output<'static>, high: bool) {
    if high {
        miso.set_high();
    } else {
        miso.set_low();
    }
}
