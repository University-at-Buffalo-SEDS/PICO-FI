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
use embassy_rp::pio::{Common, Config, Direction, FifoJoin, Pio, ShiftDirection, StateMachine};
use embassy_rp::{Peri, pio};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use heapless::String;
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
        sm1,
        ..
    } = Pio::new(pio1, crate::Irqs);

    let (mut rx_sm, mut tx_sm) = configure_spi_slave_sms(&mut common, sm0, sm1, sclk, miso, mosi);
    let mut tx_dma = Channel::new(tx_dma, crate::Irqs);
    let mut rx_dma = Channel::new(rx_dma, crate::Irqs);
    let cs = Input::new(cs, Pull::Up);

    let mut staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
    let echo_mode = matches!(bridge_config.upstream_mode, crate::config::UpstreamMode::SpiEcho);

    loop {
        if !echo_mode {
            if let Ok(resp) = rx_resp.try_receive() {
                staged_tx = resp.data;
            }
        }

        let mut rx_words = [0u32; FRAME_SIZE];
        rx_sm.clear_fifos();
        tx_sm.clear_fifos();
        rx_sm.restart();
        tx_sm.restart();
        rx_sm.set_enable(true);
        tx_sm.set_enable(true);

        {
            let rx_transfer = rx_sm.rx().dma_pull(&mut rx_dma, &mut rx_words, false);
            let tx_transfer = tx_sm.tx().dma_push(&mut tx_dma, &staged_tx, false);
            join(tx_transfer, rx_transfer).await;
        }

        while cs.is_low() {
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        rx_sm.set_enable(false);
        tx_sm.set_enable(false);

        let tx_complete = true;
        if tx_complete {
            staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        }

        let mut rx_frame = [0u8; FRAME_SIZE];
        for (dst, word) in rx_frame.iter_mut().zip(rx_words.iter()) {
            *dst = *word as u8;
        }

        if echo_mode {
            staged_tx = rx_frame;
            continue;
        }

        if let Some(next) = finalize_transaction(
            rx_frame,
            rx_words,
            bridge_config,
            link_active,
            tx,
        )
        .await
        {
            staged_tx = next;
        }
    }
}

fn configure_spi_slave_sms(
    common: &mut Common<'static, PIO1>,
    mut rx_sm: StateMachine<'static, PIO1, 0>,
    mut tx_sm: StateMachine<'static, PIO1, 1>,
    sclk: Peri<'static, PIN_10>,
    miso: Peri<'static, PIN_11>,
    mosi: Peri<'static, PIN_12>,
) -> (StateMachine<'static, PIO1, 0>, StateMachine<'static, PIO1, 1>) {
    let rx_program = pio::program::pio_asm!(
        ".wrap_target",
        "wait 0 gpio 13",
        "bitloop:",
        "wait 1 gpio 10",
        "in pins, 1",
        "wait 0 gpio 10",
        "jmp bitloop",
        ".wrap",
    );
    let tx_program = pio::program::pio_asm!(
        ".wrap_target",
        "wait 0 gpio 13",
        "bitloop:",
        "out pins, 1",
        "wait 1 gpio 10",
        "wait 0 gpio 10",
        "jmp bitloop",
        ".wrap",
    );

    let loaded_rx = common.load_program(&rx_program.program);
    let loaded_tx = common.load_program(&tx_program.program);
    let _sclk_pin = common.make_pio_pin(sclk);
    let mosi_pin = common.make_pio_pin(mosi);
    let miso_pin = common.make_pio_pin(miso);

    let mut rx_cfg = Config::default();
    rx_cfg.use_program(&loaded_rx, &[]);
    rx_cfg.set_in_pins(&[&mosi_pin]);
    rx_cfg.fifo_join = FifoJoin::RxOnly;
    rx_cfg.shift_in.auto_fill = true;
    rx_cfg.shift_in.direction = ShiftDirection::Left;
    rx_cfg.shift_in.threshold = 8;
    rx_cfg.clock_divider = 1u8.into();
    rx_sm.set_config(&rx_cfg);
    rx_sm.set_pin_dirs(Direction::In, &[&mosi_pin]);
    rx_sm.set_enable(false);

    let mut tx_cfg = Config::default();
    tx_cfg.use_program(&loaded_tx, &[]);
    tx_cfg.set_out_pins(&[&miso_pin]);
    tx_cfg.fifo_join = FifoJoin::TxOnly;
    tx_cfg.shift_out.auto_fill = true;
    tx_cfg.shift_out.direction = ShiftDirection::Left;
    tx_cfg.shift_out.threshold = 8;
    tx_cfg.clock_divider = 1u8.into();
    tx_sm.set_config(&tx_cfg);
    tx_sm.set_pins(Level::Low, &[&miso_pin]);
    tx_sm.set_pin_dirs(Direction::Out, &[&miso_pin]);
    tx_sm.set_enable(false);

    (rx_sm, tx_sm)
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

fn render_spi_capture(
    frame: &[u8; FRAME_SIZE],
    rx_words: &[u32; FRAME_SIZE],
) -> String<192> {
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

fn push_hex32(out: &mut String<192>, value: u32) {
    push_hex(out, ((value >> 24) & 0xff) as u8);
    push_hex(out, ((value >> 16) & 0xff) as u8);
    push_hex(out, ((value >> 8) & 0xff) as u8);
    push_hex(out, (value & 0xff) as u8);
}
