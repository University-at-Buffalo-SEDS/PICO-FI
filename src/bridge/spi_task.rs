//! Hardware-SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, signal_led_activity, trim_ascii_line};
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    make_response_frame, parse_request_frame, RequestFrame, FRAME_SIZE, REQ_COMMAND_MAGIC,
    REQ_DATA_MAGIC, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC,
};
use embassy_executor::Spawner;
use embassy_rp::gpio::Level;
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{
    Common, Config as PioConfig, Direction as PioDirection, FifoJoin, Pio, ShiftDirection,
    StateMachine,
};
use embassy_rp::Peri;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use portable_atomic::AtomicBool;

const SPI_IDLE_BACKOFF: Duration = Duration::from_micros(5);
const SPI_TRAILING_DRAIN_SPINS: usize = 128;
const SPI_PIO_TX_SM: usize = 1;
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
    let _tx_dma = _tx_dma;
    let _rx_dma = _rx_dma;
    let _spawner = _spawner;

    let pio = Pio::new(_pio1, crate::Irqs);
    let mut common = pio.common;
    let mut tx_sm = pio.sm1;
    let tx_program = PioSpiSlaveTxProgram::new(&mut common);

    configure_pio_spi_slave(
        &mut common,
        &mut tx_sm,
        _sclk,
        _miso,
        _mosi,
        _cs,
        &tx_program,
    );

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
    let mut rx_state = SoftwareSpiRxState::new();
    let mut transaction_armed = false;

    loop {
        if static_mode {
            transport.stage_response(static_frame);
            transaction_armed = false;
        } else if !echo_mode {
            if let Ok(resp) = rx_resp.try_receive() {
                transport.stage_response(resp.data);
                transaction_armed = false;
            }
        }

        while !spi_cs_asserted() {
            if static_mode {
                transport.stage_response(static_frame);
                transaction_armed = false;
            } else if !echo_mode {
                if let Ok(resp) = rx_resp.try_receive() {
                    transport.stage_response(resp.data);
                    transaction_armed = false;
                }
            }
            if !transaction_armed {
                arm_pio_spi_transaction(&mut tx_sm, &mut transport);
                rx_state.begin();
                transaction_armed = true;
            }
            Timer::after(SPI_IDLE_BACKOFF).await;
        }

        while spi_cs_asserted() {
            service_pio_tx_fifo(&mut tx_sm, &mut transport);
            service_software_spi_rx(&mut rx_state, &mut transport);
            core::hint::spin_loop();
        }

        for _ in 0..SPI_TRAILING_DRAIN_SPINS {
            let mut did_work = false;
            did_work |= service_pio_tx_fifo(&mut tx_sm, &mut transport);
            did_work |= service_software_spi_rx(&mut rx_state, &mut transport);
            if !did_work {
                break;
            }
        }

        tx_sm.set_enable(false);
        tx_sm.clear_fifos();
        tx_sm.restart();
        transaction_armed = false;
        let result = transport.finish_transaction();

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

struct PioSpiSlaveTxProgram<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
}

impl<'d> PioSpiSlaveTxProgram<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .wrap_target
                wait 0 gpio 13
                wait 0 gpio 10
                pull ifempty block
                out pins, 1
            bitloop:
                wait 1 gpio 10
                wait 0 gpio 10
                jmp pin, done
                pull ifempty block
                out pins, 1
                jmp bitloop
            done:
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
    tx_sm: &mut StateMachine<'d, PIO1, SPI_PIO_TX_SM>,
    sclk: Peri<'d, PIN_10>,
    miso: Peri<'d, PIN_11>,
    mosi: Peri<'d, PIN_12>,
    cs: Peri<'d, PIN_13>,
    tx_program: &PioSpiSlaveTxProgram<'d>,
) {
    let sclk_pin = common.make_pio_pin(sclk);
    let miso_pin = common.make_pio_pin(miso);
    let _mosi_pin = common.make_pio_pin(mosi);
    let cs_pin = common.make_pio_pin(cs);

    let mut tx_cfg = PioConfig::default();
    tx_cfg.use_program(&tx_program.loaded, &[]);
    tx_cfg.set_out_pins(&[&miso_pin]);
    tx_cfg.set_jmp_pin(&cs_pin);
    tx_cfg.shift_in.auto_fill = false;
    tx_cfg.shift_in.direction = ShiftDirection::Left;
    tx_cfg.shift_in.threshold = 32;
    tx_cfg.shift_out.auto_fill = false;
    tx_cfg.shift_out.direction = ShiftDirection::Left;
    tx_cfg.shift_out.threshold = 32;
    tx_cfg.fifo_join = FifoJoin::TxOnly;
    tx_cfg.clock_divider = 1u8.into();
    tx_sm.set_config(&tx_cfg);
    tx_sm.set_pins(Level::Low, &[&miso_pin]);
    tx_sm.set_pin_dirs(PioDirection::Out, &[&miso_pin]);
    tx_sm.set_pin_dirs(PioDirection::In, &[&sclk_pin, &cs_pin]);
    tx_sm.set_enable(false);
}

fn arm_pio_spi_transaction(
    tx_sm: &mut StateMachine<'_, PIO1, SPI_PIO_TX_SM>,
    transport: &mut PioSpiTransportState,
) {
    tx_sm.set_enable(false);
    tx_sm.clear_fifos();
    tx_sm.restart();
    transport.begin_transaction();
    preload_pio_tx_fifo(tx_sm, transport);
    rp_pac::PIO1.ctrl().modify(|w| {
        w.set_sm_restart(1u8 << SPI_PIO_TX_SM);
        w.set_sm_enable(w.sm_enable() | (1u8 << SPI_PIO_TX_SM));
    });
}

fn preload_pio_tx_fifo(
    tx_sm: &mut StateMachine<'_, PIO1, SPI_PIO_TX_SM>,
    transport: &mut PioSpiTransportState,
) {
    let _ = service_pio_tx_fifo(tx_sm, transport);
}

fn service_pio_tx_fifo(
    tx_sm: &mut StateMachine<'_, PIO1, SPI_PIO_TX_SM>,
    transport: &mut PioSpiTransportState,
) -> bool {
    let mut did_work = false;
    let tx = tx_sm.tx();
    while !tx.full() {
        let word = u32::from_be_bytes([
            transport.next_tx_byte_shifted_left_1(),
            transport.next_tx_byte_shifted_left_1(),
            transport.next_tx_byte_shifted_left_1(),
            transport.next_tx_byte_shifted_left_1(),
        ]);
        if !tx.try_push(word) {
            break;
        }
        did_work = true;
    }
    did_work
}

#[derive(Clone, Copy, Default)]
struct SoftwareSpiRxState {
    last_clk_high: bool,
    current_byte: u8,
    bits: u8,
}

impl SoftwareSpiRxState {
    fn new() -> Self {
        Self::default()
    }

    fn begin(&mut self) {
        self.last_clk_high = read_pin(SPI_SCK_PIN);
        self.current_byte = 0;
        self.bits = 0;
    }
}

fn service_software_spi_rx(
    state: &mut SoftwareSpiRxState,
    transport: &mut PioSpiTransportState,
) -> bool {
    let clk_high = read_pin(SPI_SCK_PIN);
    let rising = !state.last_clk_high && clk_high;
    state.last_clk_high = clk_high;
    if !rising {
        return false;
    }

    state.current_byte = (state.current_byte << 1) | (read_pin(SPI_MOSI_PIN) as u8);
    state.bits += 1;
    if state.bits == 8 {
        transport.capture_rx_word(u32::from(state.current_byte));
        state.current_byte = 0;
        state.bits = 0;
    }
    true
}

async fn finalize_transaction(
    result: TransactionResult,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) -> Option<[u8; FRAME_SIZE]> {
    let frame = match result {
        TransactionResult::IdlePoll {
            received,
            raw_words,
            ..
        } => {
            let raw = raw_words[0].to_be_bytes();
            let payload = [
                b'R',
                received.min(255) as u8,
                raw[0],
                raw[1],
                raw[2],
                raw[3],
            ];
            return Some(make_response_frame(RESP_COMMAND_MAGIC, &payload));
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

fn read_pin(pin: usize) -> bool {
    let bank0_inputs = rp_pac::SIO.gpio_in(0).read();
    ((bank0_inputs >> pin) & 1) != 0
}
