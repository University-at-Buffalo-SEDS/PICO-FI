//! Dedicated SPI slave polling task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_futures::yield_now;
use embassy_rp::Peri;
use embassy_rp::peripherals::{PIN_10, PIN_11, PIN_12, PIN_13, SPI1};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use portable_atomic::AtomicBool;

const SPI_CHUNK_PAD: u8 = 0x00;
const SPI_CS_PIN: usize = 13;

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the SPI1 slave bus and bridges framed requests.
#[allow(clippy::too_many_arguments)]
pub async fn spi_poll_task(
    _spi: Peri<'static, SPI1>,
    _sclk: Peri<'static, PIN_10>,
    _miso: Peri<'static, PIN_11>,
    _mosi: Peri<'static, PIN_12>,
    _cs: Peri<'static, PIN_13>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> ! {
    init_spi1_slave();

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

        let cs_low = spi_cs_asserted();
        if cs_low && !cs_active {
            cs_active = true;
            rx_pos = 0;
            rx_expected = FRAME_SIZE;
            drain_rx_fifo();
            fill_tx_fifo(&tx_frame, &mut tx_pos);
        }

        if cs_low {
            fill_tx_fifo(&tx_frame, &mut tx_pos);
            read_rx_fifo(&mut rx_frame, &mut rx_pos, &mut rx_expected);
        } else if cs_active {
            cs_active = false;
            drain_rx_fifo_into(&mut rx_frame, &mut rx_pos, &mut rx_expected);

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
            }

            if tx_pos >= FRAME_SIZE {
                tx_frame = make_response_frame(RESP_DATA_MAGIC, b"");
                tx_pos = 0;
            }

            rx_frame = [0u8; FRAME_SIZE];
            rx_pos = 0;
            rx_expected = FRAME_SIZE;
        }

        yield_now().await;
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
            let response = render_local_bridge_command(bridge_config, link_active, line);
            Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()))
        }
        _ => None,
    }
}

fn read_rx_fifo(frame: &mut [u8; FRAME_SIZE], pos: &mut usize, expected: &mut usize) {
    let spi = rp_pac::SPI1;
    while spi.sr().read().rne() {
        let byte = spi.dr().read().data() as u8;
        append_byte(byte, frame, pos, expected);
    }
}

fn drain_rx_fifo_into(frame: &mut [u8; FRAME_SIZE], pos: &mut usize, expected: &mut usize) {
    let spi = rp_pac::SPI1;
    while spi.sr().read().rne() {
        let byte = spi.dr().read().data() as u8;
        append_byte(byte, frame, pos, expected);
    }
}

fn drain_rx_fifo() {
    let spi = rp_pac::SPI1;
    while spi.sr().read().rne() {
        let _ = spi.dr().read().data();
    }
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

fn fill_tx_fifo(frame: &[u8; FRAME_SIZE], tx_pos: &mut usize) {
    let spi = rp_pac::SPI1;
    while spi.sr().read().tnf() {
        let byte = if *tx_pos < FRAME_SIZE {
            let out = frame[*tx_pos];
            *tx_pos += 1;
            out
        } else {
            SPI_CHUNK_PAD
        };
        spi.dr().write(|w| w.set_data(byte as u16));
    }
}

fn spi_cs_asserted() -> bool {
    let sio = rp_pac::SIO;
    let bank0_inputs = sio.gpio_in(0).read();
    ((bank0_inputs >> SPI_CS_PIN) & 1) == 0
}

fn init_spi1_slave() {
    let resets = rp_pac::RESETS;
    let mut reset = resets.reset().read();
    reset.set_spi1(true);
    resets.reset().write_value(reset);
    reset.set_spi1(false);
    resets.reset().write_value(reset);
    while !resets.reset_done().read().spi1() {}

    configure_spi_pin(10);
    configure_spi_pin(11);
    configure_spi_pin(12);
    configure_spi_pin(13);

    let spi = rp_pac::SPI1;
    spi.cr1().write(|w| w.set_sse(false));
    spi.imsc().write_value(Default::default());
    spi.dmacr().write_value(Default::default());
    spi.icr().write(|w| {
        w.set_roric(true);
        w.set_rtic(true);
    });
    spi.cpsr().write(|w| w.set_cpsdvsr(2));
    spi.cr0().write(|w| {
        w.set_dss(0b0111);
        w.set_frf(0);
        w.set_spo(false);
        w.set_sph(false);
        w.set_scr(0);
    });
    spi.cr1().write(|w| {
        w.set_lbm(false);
        w.set_sse(true);
        w.set_ms(true);
        w.set_sod(false);
    });
}

fn configure_spi_pin(pin: usize) {
    let io = rp_pac::IO_BANK0;
    io.gpio(pin).ctrl().write(|w| w.set_funcsel(1));

    let pads = rp_pac::PADS_BANK0;
    pads.gpio(pin).write(|w| {
        w.set_ie(true);
        w.set_od(false);
        w.set_pue(false);
        w.set_pde(false);
        w.set_schmitt(true);
        w.set_slewfast(false);
    });
}
