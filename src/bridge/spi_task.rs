//! Hardware-SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embedded_hal::spi::MODE_0;
use embedded_hal_nb::spi::FullDuplex;
use embassy_executor::Spawner;
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use heapless::String;
use portable_atomic::AtomicBool;
use rp2040_hal::gpio::{FunctionSpi, Pins};
use rp2040_hal::pac;
use rp2040_hal::sio::Sio;
use rp2040_hal::spi::{Enabled, Spi, SpiDevice, ValidSpiPinout};

const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(5);
const SPI_TRAILING_DRAIN_SPINS: usize = 128;
const SPI_CS_PIN: usize = 13;

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
    let mut pac = unsafe { pac::Peripherals::steal() };
    let sio = Sio::new(pac.SIO);
    let pins = Pins::new(pac.IO_BANK0, pac.PADS_BANK0, sio.gpio_bank0, &mut pac.RESETS);
    let spi_pins = (
        pins.gpio11.into_function::<FunctionSpi>(),
        pins.gpio12.into_function::<FunctionSpi>(),
        pins.gpio10.into_function::<FunctionSpi>(),
        pins.gpio13.into_function::<FunctionSpi>(),
    );
    let mut spi = Spi::<_, _, _, 8>::new(pac.SPI1, spi_pins).init_slave(&mut pac.RESETS, MODE_0);

    let echo_mode = matches!(bridge_config.upstream_mode, crate::config::UpstreamMode::SpiEcho);
    let static_mode = matches!(bridge_config.upstream_mode, crate::config::UpstreamMode::SpiStatic);
    let mut staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
    let mut staged_tx_pos = 0usize;
    let static_frame = make_response_frame(RESP_COMMAND_MAGIC, b"pong");

    loop {
        if static_mode {
            staged_tx = static_frame;
            staged_tx_pos = 0;
        } else if !echo_mode {
            if let Ok(resp) = rx_resp.try_receive() {
                staged_tx = resp.data;
                staged_tx_pos = 0;
            }
        }

        while !spi_cs_asserted() {
            if static_mode {
                staged_tx = static_frame;
                staged_tx_pos = 0;
            } else if !echo_mode {
                if let Ok(resp) = rx_resp.try_receive() {
                    staged_tx = resp.data;
                    staged_tx_pos = 0;
                }
            }
            service_spi_tx_fifo(&mut spi, &staged_tx, &mut staged_tx_pos);
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        let mut rx_frame = [0u8; FRAME_SIZE];
        let mut rx_words = [0u32; FRAME_SIZE];
        let mut rx_pos = 0usize;

        while spi_cs_asserted() {
            service_spi_tx_fifo(&mut spi, &staged_tx, &mut staged_tx_pos);
            service_spi_rx_fifo(&mut spi, &mut rx_frame, &mut rx_words, &mut rx_pos);
            core::hint::spin_loop();
        }

        for _ in 0..SPI_TRAILING_DRAIN_SPINS {
            let mut did_work = false;
            did_work |= service_spi_tx_fifo(&mut spi, &staged_tx, &mut staged_tx_pos);
            did_work |= service_spi_rx_fifo(&mut spi, &mut rx_frame, &mut rx_words, &mut rx_pos);
            if !did_work && !spi.is_busy() {
                break;
            }
        }

        if staged_tx_pos >= FRAME_SIZE {
            staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
            staged_tx_pos = 0;
        }

        if static_mode {
            continue;
        }

        if echo_mode {
            staged_tx = rx_frame;
            staged_tx_pos = 0;
            continue;
        }

        if let Some(next) =
            finalize_transaction(rx_frame, rx_words, bridge_config, link_active, tx).await
        {
            staged_tx = next;
            staged_tx_pos = 0;
        }
    }
}

fn service_spi_tx_fifo<D, P>(
    spi: &mut Spi<Enabled, D, P, 8>,
    staged_tx: &[u8; FRAME_SIZE],
    tx_pos: &mut usize,
) -> bool
where
    D: SpiDevice,
    P: ValidSpiPinout<D>,
{
    let mut did_work = false;
    while *tx_pos < FRAME_SIZE {
        if spi.write(staged_tx[*tx_pos]).is_err() {
            break;
        }
        *tx_pos += 1;
        did_work = true;
    }
    did_work
}

fn service_spi_rx_fifo<D, P>(
    spi: &mut Spi<Enabled, D, P, 8>,
    rx_frame: &mut [u8; FRAME_SIZE],
    rx_words: &mut [u32; FRAME_SIZE],
    rx_pos: &mut usize,
) -> bool
where
    D: SpiDevice,
    P: ValidSpiPinout<D>,
{
    let mut did_work = false;
    while let Ok(byte) = spi.read() {
        if *rx_pos < FRAME_SIZE {
            rx_frame[*rx_pos] = byte;
            rx_words[*rx_pos] = byte as u32;
            *rx_pos += 1;
        }
        did_work = true;
    }
    did_work
}

async fn finalize_transaction(
    frame: [u8; FRAME_SIZE],
    rx_words: [u32; FRAME_SIZE],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> Option<[u8; FRAME_SIZE]> {
    if frame[0] == 0 && frame[1] == 0 {
        let nonzero = frame.iter().filter(|&&b| b != 0).count();
        if nonzero == 0 {
            return Some(make_response_frame(
                RESP_COMMAND_MAGIC,
                render_spi_capture(&frame, &rx_words).as_bytes(),
            ));
        }
        return None;
    }

    match parse_request_frame(&frame) {
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response = render_local_bridge_command(bridge_config, link_active, line);
            Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()))
        }
        Some(RequestFrame::Data(_)) => {
            tx.send(SpiFrame { data: frame }).await;
            None
        }
        None => Some(make_response_frame(
            RESP_COMMAND_MAGIC,
            render_spi_capture(&frame, &rx_words).as_bytes(),
        )),
    }
}

fn render_spi_capture(frame: &[u8; FRAME_SIZE], rx_words: &[u32; FRAME_SIZE]) -> String<192> {
    let mut out = String::<192>::new();
    let nonzero = frame.iter().filter(|&&b| b != 0).count();
    let _ = out.push_str("spi rx nz=");
    push_usize(&mut out, nonzero);
    let _ = out.push_str(" b=");
    for (index, byte) in frame.iter().take(8).enumerate() {
        if index != 0 {
            let _ = out.push('-');
        }
        push_hex(&mut out, *byte);
    }
    let _ = out.push_str(" w=");
    for (index, word) in rx_words.iter().take(4).enumerate() {
        if index != 0 {
            let _ = out.push('-');
        }
        push_hex32(&mut out, *word);
    }
    out
}

fn push_hex(out: &mut String<192>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}

fn push_hex32(out: &mut String<192>, value: u32) {
    push_hex(out, ((value >> 24) & 0xff) as u8);
    push_hex(out, ((value >> 16) & 0xff) as u8);
    push_hex(out, ((value >> 8) & 0xff) as u8);
    push_hex(out, (value & 0xff) as u8);
}

fn push_usize(out: &mut String<192>, mut value: usize) {
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

fn spi_cs_asserted() -> bool {
    let bank0_inputs = rp_pac::SIO.gpio_in(0).read();
    ((bank0_inputs >> SPI_CS_PIN) & 1) == 0
}
