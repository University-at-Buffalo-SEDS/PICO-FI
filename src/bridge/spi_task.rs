//! Hardware-SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_executor::Spawner;
use embassy_rp::gpio::Level;
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{Common, Config as PioConfig, Direction as PioDirection, Pio, ShiftDirection, StateMachine};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use heapless::String;
use portable_atomic::AtomicBool;

const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(5);
const SPI_TRAILING_DRAIN_SPINS: usize = 128;
const SPI_PIO_SM: usize = 0;
const SPI_CS_PIN: usize = 13;
const SPI_SCK_PIN: usize = 10;

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
    let pio = Pio::new(_pio1, crate::Irqs);
    let mut common = pio.common;
    let mut sm = pio.sm0;
    let program = PioSpiSlaveProgram::new(&mut common);

    configure_pio_spi_slave(&mut common, &mut sm, _sclk, _miso, _mosi, _cs, &program);

    let echo_mode = matches!(bridge_config.upstream_mode, crate::config::UpstreamMode::SpiEcho);
    let static_mode = matches!(bridge_config.upstream_mode, crate::config::UpstreamMode::SpiStatic);
    let mut staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
    let static_frame = make_response_frame(RESP_COMMAND_MAGIC, b"pong");
    let mut transaction_armed = false;
    let mut armed_tx_pos = 0usize;

    loop {
        if static_mode {
            staged_tx = static_frame;
            transaction_armed = false;
        } else if !echo_mode {
            if let Ok(resp) = rx_resp.try_receive() {
                staged_tx = resp.data;
                transaction_armed = false;
            }
        }

        while !spi_cs_asserted() {
            if static_mode {
                staged_tx = static_frame;
                transaction_armed = false;
            } else if !echo_mode {
                if let Ok(resp) = rx_resp.try_receive() {
                    staged_tx = resp.data;
                    transaction_armed = false;
                }
            }
            if !transaction_armed {
                armed_tx_pos = arm_pio_spi_transaction(&mut sm, &staged_tx);
                transaction_armed = true;
            }
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        let mut rx_frame = [0u8; FRAME_SIZE];
        let mut rx_words = [0u32; FRAME_SIZE];
        let mut rx_pos = 0usize;
        let mut tx_pos = armed_tx_pos;

        while spi_cs_asserted() {
            tx_pos = service_pio_tx_fifo(&mut sm, &staged_tx, tx_pos);
            service_pio_rx_fifo(&mut sm, &mut rx_frame, &mut rx_words, &mut rx_pos);
            core::hint::spin_loop();
        }

        for _ in 0..SPI_TRAILING_DRAIN_SPINS {
            let mut did_work = false;
            let next_tx_pos = service_pio_tx_fifo(&mut sm, &staged_tx, tx_pos);
            did_work |= next_tx_pos != tx_pos;
            tx_pos = next_tx_pos;
            did_work |= service_pio_rx_fifo(&mut sm, &mut rx_frame, &mut rx_words, &mut rx_pos);
            if !did_work {
                break;
            }
        }

        sm.set_enable(false);
        sm.clear_fifos();
        sm.restart();
        transaction_armed = false;

        if tx_pos >= FRAME_SIZE {
            staged_tx = make_response_frame(RESP_DATA_MAGIC, b"");
        }

        if static_mode {
            continue;
        }

        if echo_mode {
            staged_tx = rx_frame;
            continue;
        }

        if let Some(next) =
            finalize_transaction(rx_frame, rx_words, bridge_config, link_active, tx).await
        {
            staged_tx = next;
        }
    }
}

struct PioSpiSlaveProgram<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
}

impl<'d> PioSpiSlaveProgram<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .wrap_target
                wait 0 gpio 13
            bitloop:
                jmp pin done
                out pins, 1
                wait 1 gpio 10
                in pins, 1
                wait 0 gpio 10
                jmp bitloop
            done:
                irq 0 rel
                wait 1 gpio 13
                .wrap
            "#
        );
        Self {
            loaded: common.load_program(&prg.program),
        }
    }
}

fn configure_pio_spi_slave<'d>(
    common: &mut Common<'d, PIO1>,
    sm: &mut StateMachine<'d, PIO1, SPI_PIO_SM>,
    sclk: Peri<'d, PIN_10>,
    miso: Peri<'d, PIN_11>,
    mosi: Peri<'d, PIN_12>,
    cs: Peri<'d, PIN_13>,
    program: &PioSpiSlaveProgram<'d>,
) {
    let sclk_pin = common.make_pio_pin(sclk);
    let miso_pin = common.make_pio_pin(miso);
    let mosi_pin = common.make_pio_pin(mosi);
    let cs_pin = common.make_pio_pin(cs);

    let bypass_mask =
        (1u32 << SPI_SCK_PIN) | (1u32 << SPI_CS_PIN) | (1u32 << (SPI_SCK_PIN + 2));
    common.set_input_sync_bypass(bypass_mask, bypass_mask);

    let mut cfg = PioConfig::default();
    cfg.use_program(&program.loaded, &[]);
    cfg.set_out_pins(&[&miso_pin]);
    cfg.set_in_pins(&[&mosi_pin]);
    cfg.set_jmp_pin(&cs_pin);
    cfg.shift_in.auto_fill = true;
    cfg.shift_in.direction = ShiftDirection::Left;
    cfg.shift_in.threshold = 8;
    cfg.shift_out.auto_fill = true;
    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.threshold = 8;
    cfg.clock_divider = 1u8.into();
    sm.set_config(&cfg);
    sm.set_pins(Level::Low, &[&miso_pin]);
    sm.set_pin_dirs(PioDirection::Out, &[&miso_pin]);
    sm.set_pin_dirs(PioDirection::In, &[&sclk_pin, &mosi_pin, &cs_pin]);
    sm.set_enable(false);
}

fn arm_pio_spi_transaction(
    sm: &mut StateMachine<'_, PIO1, SPI_PIO_SM>,
    staged_tx: &[u8; FRAME_SIZE],
) -> usize {
    sm.set_enable(false);
    sm.clear_fifos();
    sm.restart();
    let tx_pos = preload_pio_tx_fifo(sm, staged_tx, 0);
    sm.set_enable(true);
    tx_pos
}

fn preload_pio_tx_fifo(
    sm: &mut StateMachine<'_, PIO1, SPI_PIO_SM>,
    staged_tx: &[u8; FRAME_SIZE],
    tx_pos: usize,
) -> usize {
    service_pio_tx_fifo(sm, staged_tx, tx_pos)
}

fn service_pio_tx_fifo(
    sm: &mut StateMachine<'_, PIO1, SPI_PIO_SM>,
    staged_tx: &[u8; FRAME_SIZE],
    mut tx_pos: usize,
) -> usize {
    let tx = sm.tx();
    while tx_pos < FRAME_SIZE {
        let word = u32::from_be_bytes([staged_tx[tx_pos], 0, 0, 0]);
        if !tx.try_push(word) {
            break;
        }
        tx_pos += 1;
    }
    tx_pos
}

fn service_pio_rx_fifo(
    sm: &mut StateMachine<'_, PIO1, SPI_PIO_SM>,
    rx_frame: &mut [u8; FRAME_SIZE],
    rx_words: &mut [u32; FRAME_SIZE],
    rx_pos: &mut usize,
) -> bool {
    let mut did_work = false;
    let rx = sm.rx();
    while let Some(word) = rx.try_pull() {
        let byte = word as u8;
        if *rx_pos < FRAME_SIZE {
            rx_frame[*rx_pos] = byte;
            rx_words[*rx_pos] = word;
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
