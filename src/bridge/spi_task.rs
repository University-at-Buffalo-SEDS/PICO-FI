//! PIO-backed SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::dma::Channel;
use embassy_rp::gpio::{Input, Level, Pull};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{Common, Config, Direction, FifoJoin, Pio, ShiftDirection};
use embassy_rp::{Peri, pio};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use portable_atomic::AtomicBool;

const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(5);

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the PIO-based SPI slave bus and bridges framed requests.
#[allow(clippy::too_many_arguments)]
pub async fn spi_poll_task(
    pio1: Peri<'static, PIO1>,
    sclk: Peri<'static, PIN_10>,
    miso: Peri<'static, PIN_11>,
    mosi: Peri<'static, PIN_12>,
    cs: Peri<'static, PIN_13>,
    tx_dma: Peri<'static, DMA_CH2>,
    rx_dma: Peri<'static, DMA_CH3>,
    _spawner: Spawner,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    rx_resp: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> ! {
    let Pio {
        mut common,
        sm0,
        ..
    } = Pio::new(pio1, crate::Irqs);

    let mut sm = configure_spi_slave_sm(&mut common, sm0, sclk, miso, mosi);
    let mut tx_dma = Channel::new(tx_dma, crate::Irqs);
    let mut rx_dma = Channel::new(rx_dma, crate::Irqs);
    let cs = Input::new(cs, Pull::Up);

    let mut staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");

    loop {
        if let Ok(resp) = rx_resp.try_receive() {
            staged_tx = resp.data;
        }

        while cs.is_high() {
            if let Ok(resp) = rx_resp.try_receive() {
                staged_tx = resp.data;
            }
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        let mut rx_frame = [0u8; FRAME_SIZE];
        sm.clear_fifos();
        sm.restart();
        sm.set_enable(true);

        {
            let (rx_sm, tx_sm) = sm.rx_tx();
            let rx_transfer = rx_sm.dma_pull(&mut rx_dma, &mut rx_frame, false);
            let tx_transfer = tx_sm.dma_push(&mut tx_dma, &staged_tx, false);
            join(tx_transfer, rx_transfer).await;
        }

        sm.set_enable(false);

        while cs.is_low() {
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        let tx_complete = true;
        if tx_complete {
            staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        }

        if let Some(next) =
            finalize_transaction(rx_frame, bridge_config, link_active, tx).await
        {
            staged_tx = next;
        }
    }
}

fn configure_spi_slave_sm(
    common: &mut Common<'static, PIO1>,
    mut sm: embassy_rp::pio::StateMachine<'static, PIO1, 0>,
    sclk: Peri<'static, PIN_10>,
    miso: Peri<'static, PIN_11>,
    mosi: Peri<'static, PIN_12>,
) -> embassy_rp::pio::StateMachine<'static, PIO1, 0> {
    let program = pio::program::pio_asm!(
        ".wrap_target",
        "wait 0 gpio 13",
        "bitloop:",
        "out pins, 1",
        "wait 1 gpio 10",
        "in pins, 1",
        "wait 0 gpio 10",
        "jmp bitloop",
        ".wrap",
    );

    let loaded = common.load_program(&program.program);
    let _sclk_pin = common.make_pio_pin(sclk);
    let mosi_pin = common.make_pio_pin(mosi);
    let miso_pin = common.make_pio_pin(miso);

    let mut cfg = Config::default();
    cfg.use_program(&loaded, &[]);
    cfg.set_out_pins(&[&miso_pin]);
    cfg.set_in_pins(&[&mosi_pin]);
    cfg.fifo_join = FifoJoin::Duplex;
    cfg.shift_in.auto_fill = true;
    cfg.shift_in.direction = ShiftDirection::Left;
    cfg.shift_in.threshold = 8;
    cfg.shift_out.auto_fill = true;
    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.threshold = 8;
    cfg.clock_divider = 1u8.into();

    sm.set_config(&cfg);
    sm.set_pins(Level::Low, &[&miso_pin]);
    sm.set_pin_dirs(Direction::Out, &[&miso_pin]);
    sm.set_pin_dirs(Direction::In, &[&mosi_pin]);
    sm.set_enable(false);
    sm
}

async fn finalize_transaction(
    frame: [u8; FRAME_SIZE],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> Option<[u8; FRAME_SIZE]> {
    if frame[0] == 0 && frame[1] == 0 {
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
            b"error invalid spi frame",
        )),
    }
}
