//! Dedicated SPI slave polling task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_futures::yield_now;
use embedded_hal::spi::MODE_0;
use embedded_hal_nb::spi::FullDuplex;
use heapless::String;
use portable_atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use rp2040_hal::gpio::{FunctionSpi, Pins};
use rp2040_hal::pac;
use rp2040_hal::sio::Sio;
use rp2040_hal::spi::{Enabled, Spi, SpiDevice, ValidSpiPinout};
use embassy_time::{Duration, Timer};

const SPI_CHUNK_PAD: u8 = 0x00;
const SPI_CS_PIN: usize = 13;
const SPI_FRAME_FORMAT: embedded_hal::spi::Mode = MODE_0;
const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(2);
const SPI_DEBUG_SAMPLE_LEN: usize = 8;
const SPI_DEBUG_FLAG_COMPLETE: u8 = 1 << 0;
const SPI_DEBUG_FLAG_COMMAND: u8 = 1 << 1;
const SPI_DEBUG_FLAG_DATA: u8 = 1 << 2;
const SPI_DEBUG_FLAG_INVALID: u8 = 1 << 3;
const SPI_DEBUG_FLAG_POLL: u8 = 1 << 4;

static SPI_DEBUG_MAGIC: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_LEN: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_POS: AtomicUsize = AtomicUsize::new(0);
static SPI_DEBUG_EXPECTED: AtomicUsize = AtomicUsize::new(0);
static SPI_DEBUG_FLAGS: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B0: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B1: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B2: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B3: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B4: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B5: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B6: AtomicU8 = AtomicU8::new(0);
static SPI_DEBUG_B7: AtomicU8 = AtomicU8::new(0);

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

trait SpiSlaveBus {
    fn try_read_byte(&mut self) -> Option<u8>;
    fn try_write_byte(&mut self, byte: u8) -> bool;
    fn is_busy(&self) -> bool;
}

impl<D, P> SpiSlaveBus for Spi<Enabled, D, P, 8>
where
    D: SpiDevice,
    P: ValidSpiPinout<D>,
{
    fn try_read_byte(&mut self) -> Option<u8> {
        match self.read() {
            Ok(byte) => Some(byte),
            Err(nb::Error::WouldBlock) => None,
            Err(nb::Error::Other(err)) => match err {},
        }
    }

    fn try_write_byte(&mut self, byte: u8) -> bool {
        match self.write(byte) {
            Ok(()) => true,
            Err(nb::Error::WouldBlock) => false,
            Err(nb::Error::Other(err)) => match err {},
        }
    }

    fn is_busy(&self) -> bool {
        Spi::is_busy(self)
    }
}

/// Continuously services the SPI1 slave bus and bridges framed requests.
pub async fn spi_poll_task(
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
    // In slave mode the RP2040 must match the master's wire format, but it does not own the bus
    // clock rate. The Pi master provides SCK; only CPOL/CPHA and data size are configured here.
    let mut spi =
        Spi::<_, _, _, 8>::new(pac.SPI1, spi_pins).init_slave(&mut pac.RESETS, SPI_FRAME_FORMAT);

    let mut rx_frame = [0u8; FRAME_SIZE];
    let mut rx_pos = 0usize;
    let mut rx_expected = FRAME_SIZE;
    let mut tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
    let mut tx_pos = 0usize;
    let mut cs_active = false;

    loop {
        if let Ok(resp) = rx_resp.try_receive() {
            tx_frame = resp.data;
            tx_pos = 0;
        }

        let mut did_work = false;
        let cs_low = spi_cs_asserted();

        if cs_low && !cs_active {
            cs_active = true;
            rx_pos = 0;
            rx_expected = FRAME_SIZE;
            tx_pos = 0;
            did_work |= read_rx_fifo(&mut spi, &mut rx_frame, &mut rx_pos, &mut rx_expected);
            did_work |= fill_tx_fifo(&mut spi, &tx_frame, &mut tx_pos);
        }

        if cs_low {
            did_work |= read_rx_fifo(&mut spi, &mut rx_frame, &mut rx_pos, &mut rx_expected);
            did_work |= fill_tx_fifo(&mut spi, &tx_frame, &mut tx_pos);
        } else if cs_active {
            cs_active = false;
            did_work |= drain_transaction_end(&mut spi, &mut rx_frame, &mut rx_pos, &mut rx_expected);

            if rx_complete(rx_pos, rx_expected) {
                record_spi_debug_frame(&rx_frame, rx_pos, rx_expected, SPI_DEBUG_FLAG_COMPLETE);
                process_complete_frame(
                    rx_frame,
                    bridge_config,
                    link_active,
                    &mut tx_frame,
                    &mut tx_pos,
                    tx,
                )
                .await;
                did_work = true;
            }

            if tx_pos >= FRAME_SIZE {
                tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
                tx_pos = 0;
            }

            rx_frame = [0u8; FRAME_SIZE];
            rx_pos = 0;
            rx_expected = FRAME_SIZE;
        }

        if !did_work {
            if cs_active || spi_cs_asserted() {
                yield_now().await;
            } else {
                Timer::after(SPI_IDLE_BACKOFF).await;
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
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) {
    // SPI read polls clock data out by sending zero-filled MOSI bytes.
    // Do not treat that readback transaction as a new upstream request.
    if frame[0] == 0 && frame[1] == 0 {
        SPI_DEBUG_FLAGS.store(SPI_DEBUG_FLAG_COMPLETE | SPI_DEBUG_FLAG_POLL, Ordering::Relaxed);
        return;
    }

    if let Some(response) = handle_local_request(frame, bridge_config, link_active) {
        *tx_frame = response;
        *tx_pos = 0;
    } else {
        tx.send(SpiFrame { data: frame }).await;
    }
}

fn handle_local_request(
    frame: [u8; FRAME_SIZE],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Option<[u8; FRAME_SIZE]> {
    match parse_request_frame(&frame) {
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            SPI_DEBUG_FLAGS.store(SPI_DEBUG_FLAG_COMPLETE | SPI_DEBUG_FLAG_COMMAND, Ordering::Relaxed);
            let response = if line == "/spidbg" {
                render_spi_debug()
            } else {
                render_local_bridge_command(bridge_config, link_active, line)
            };
            Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()))
        }
        Some(RequestFrame::Data(_)) => {
            SPI_DEBUG_FLAGS.store(SPI_DEBUG_FLAG_COMPLETE | SPI_DEBUG_FLAG_DATA, Ordering::Relaxed);
            None
        }
        None => {
            SPI_DEBUG_FLAGS.store(SPI_DEBUG_FLAG_COMPLETE | SPI_DEBUG_FLAG_INVALID, Ordering::Relaxed);
            None
        }
    }
}

fn read_rx_fifo<S: SpiSlaveBus>(
    spi: &mut S,
    frame: &mut [u8; FRAME_SIZE],
    pos: &mut usize,
    expected: &mut usize,
) -> bool {
    let mut did_work = false;
    while let Some(byte) = spi.try_read_byte() {
        append_byte(byte, frame, pos, expected);
        did_work = true;
    }
    did_work
}

fn drain_transaction_end<S: SpiSlaveBus>(
    spi: &mut S,
    frame: &mut [u8; FRAME_SIZE],
    pos: &mut usize,
    expected: &mut usize,
) -> bool {
    let mut did_work = read_rx_fifo(spi, frame, pos, expected);
    for _ in 0..64 {
        let drained = read_rx_fifo(spi, frame, pos, expected);
        did_work |= drained;
        if rx_complete(*pos, *expected) || (!spi.is_busy() && !drained) {
            break;
        }
        core::hint::spin_loop();
    }
    did_work
}

fn append_byte(byte: u8, frame: &mut [u8; FRAME_SIZE], pos: &mut usize, expected: &mut usize) {
    if *pos < FRAME_SIZE {
        frame[*pos] = byte;
        *pos += 1;
        if *pos >= 2 {
            *expected = (frame[1] as usize + 2).min(FRAME_SIZE);
        }
    }
}

fn rx_complete(rx_pos: usize, rx_expected: usize) -> bool {
    rx_pos >= 2 && rx_pos >= rx_expected
}

fn fill_tx_fifo<S: SpiSlaveBus>(spi: &mut S, frame: &[u8; FRAME_SIZE], tx_pos: &mut usize) -> bool {
    let mut did_work = false;
    loop {
        let byte = if *tx_pos < FRAME_SIZE {
            frame[*tx_pos]
        } else {
            SPI_CHUNK_PAD
        };
        if !spi.try_write_byte(byte) {
            break;
        }
        if *tx_pos < FRAME_SIZE {
            *tx_pos += 1;
        }
        did_work = true;
    }
    did_work
}

fn spi_cs_asserted() -> bool {
    let bank0_inputs = rp2040_hal::sio::Sio::read_bank0();
    ((bank0_inputs >> SPI_CS_PIN) & 1) == 0
}

fn record_spi_debug_frame(
    frame: &[u8; FRAME_SIZE],
    pos: usize,
    expected: usize,
    flags: u8,
) {
    SPI_DEBUG_MAGIC.store(frame[0], Ordering::Relaxed);
    SPI_DEBUG_LEN.store(frame[1], Ordering::Relaxed);
    SPI_DEBUG_POS.store(pos, Ordering::Relaxed);
    SPI_DEBUG_EXPECTED.store(expected, Ordering::Relaxed);
    SPI_DEBUG_FLAGS.store(flags, Ordering::Relaxed);
    SPI_DEBUG_B0.store(frame[0], Ordering::Relaxed);
    SPI_DEBUG_B1.store(frame[1], Ordering::Relaxed);
    SPI_DEBUG_B2.store(frame[2], Ordering::Relaxed);
    SPI_DEBUG_B3.store(frame[3], Ordering::Relaxed);
    SPI_DEBUG_B4.store(frame[4], Ordering::Relaxed);
    SPI_DEBUG_B5.store(frame[5], Ordering::Relaxed);
    SPI_DEBUG_B6.store(frame[6], Ordering::Relaxed);
    SPI_DEBUG_B7.store(frame[7], Ordering::Relaxed);
}

fn render_spi_debug() -> String<192> {
    let mut out = String::<192>::new();
    let flags = SPI_DEBUG_FLAGS.load(Ordering::Relaxed);
    let _ = out.push_str("m=");
    push_hex_u8(&mut out, SPI_DEBUG_MAGIC.load(Ordering::Relaxed));
    let _ = out.push_str(" len=");
    push_u8_dec(&mut out, SPI_DEBUG_LEN.load(Ordering::Relaxed));
    let _ = out.push_str(" pos=");
    push_usize_dec(&mut out, SPI_DEBUG_POS.load(Ordering::Relaxed));
    let _ = out.push_str(" exp=");
    push_usize_dec(&mut out, SPI_DEBUG_EXPECTED.load(Ordering::Relaxed));
    let _ = out.push_str(" flags=");
    if flags & SPI_DEBUG_FLAG_POLL != 0 {
        let _ = out.push_str("poll");
    } else if flags & SPI_DEBUG_FLAG_COMMAND != 0 {
        let _ = out.push_str("cmd");
    } else if flags & SPI_DEBUG_FLAG_DATA != 0 {
        let _ = out.push_str("data");
    } else if flags & SPI_DEBUG_FLAG_INVALID != 0 {
        let _ = out.push_str("invalid");
    } else if flags & SPI_DEBUG_FLAG_COMPLETE != 0 {
        let _ = out.push_str("complete");
    } else {
        let _ = out.push_str("none");
    }
    let _ = out.push_str(" b=");
    for idx in 0..SPI_DEBUG_SAMPLE_LEN {
        if idx != 0 {
            let _ = out.push('-');
        }
        push_hex_u8(&mut out, spi_debug_byte(idx));
    }
    out
}

fn spi_debug_byte(index: usize) -> u8 {
    match index {
        0 => SPI_DEBUG_B0.load(Ordering::Relaxed),
        1 => SPI_DEBUG_B1.load(Ordering::Relaxed),
        2 => SPI_DEBUG_B2.load(Ordering::Relaxed),
        3 => SPI_DEBUG_B3.load(Ordering::Relaxed),
        4 => SPI_DEBUG_B4.load(Ordering::Relaxed),
        5 => SPI_DEBUG_B5.load(Ordering::Relaxed),
        6 => SPI_DEBUG_B6.load(Ordering::Relaxed),
        _ => SPI_DEBUG_B7.load(Ordering::Relaxed),
    }
}

fn push_hex_u8(out: &mut String<192>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}

fn push_u8_dec(out: &mut String<192>, value: u8) {
    push_usize_dec(out, value as usize);
}

fn push_usize_dec(out: &mut String<192>, mut value: usize) {
    let mut buf = [0u8; 20];
    let mut len = 0usize;
    if value == 0 {
        let _ = out.push('0');
        return;
    }
    while value > 0 {
        buf[len] = (value % 10) as u8;
        value /= 10;
        len += 1;
    }
    while len > 0 {
        len -= 1;
        let _ = out.push((b'0' + buf[len]) as char);
    }
}
