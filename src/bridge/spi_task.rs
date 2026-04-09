//! PIO-backed SPI slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, signal_led_activity, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, REQ_COMMAND_MAGIC, REQ_DATA_MAGIC, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC,
    RequestFrame, make_response_frame, parse_request_frame,
};
use embassy_executor::Spawner;
use embassy_rp::Peri;
use embassy_rp::dma::Channel;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{
    Common, Config as PioConfig, Direction as PioDirection, Pio, ShiftDirection,
};
use embassy_time::Timer;
use portable_atomic::AtomicBool;

const SPI_PIO_CS_SM: usize = 1;
const SPI_PIO_IO_SM: usize = 2;
const SPI_IRQ_CS_FALLING: usize = 1;
const SPI_IRQ_CS_RISING: usize = 2;

/// Message type for framed SPI transfers passed between the bus task and bridge session.
#[derive(Clone, Copy)]
pub struct SpiFrame {
    pub data: [u8; FRAME_SIZE],
}

/// Continuously services the SPI1 slave bus and bridges framed requests.
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
    tx: &'static OverwriteQueue<SpiFrame, 8>,
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
) -> ! {
    if matches!(
        bridge_config.upstream_mode,
        crate::config::UpstreamMode::SpiLineHigh
    ) {
        drop(pio1);
        drop(sclk);
        drop(mosi);
        drop(cs);
        drop(tx_dma);
        drop(rx_dma);
        let mut miso = Output::new(miso, Level::High);
        loop {
            miso.set_high();
            Timer::after_secs(1).await;
        }
    }

    let mut tx_dma = Channel::new(tx_dma, crate::Irqs);
    let mut rx_dma = Channel::new(rx_dma, crate::Irqs);

    let pio = Pio::new(pio1, crate::Irqs);
    let mut common = pio.common;
    let irq_flags = pio.irq_flags.clone();
    let mut cs_irq = pio.irq1;
    let mut cs_release_irq = pio.irq2;
    let mut cs_sm = pio.sm1;
    let mut io_sm = pio.sm2;

    let cs_program = PioSpiCsProgram::new(&mut common);
    let io_program = PioSpiMode1Program::new(&mut common);
    configure_cs_sm(&mut common, &mut cs_sm, cs, &cs_program);
    configure_io_sm(&mut common, &mut io_sm, sclk, miso, mosi, &io_program);

    irq_flags.clear_all(0xff);
    cs_sm.set_enable(true);
    io_sm.set_enable(false);
    io_sm.clear_fifos();
    io_sm.restart();

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
    let mut rx_frame = [0u8; FRAME_SIZE];

    loop {
        if static_mode {
            transport.stage_response(static_frame);
        } else if !echo_mode {
            if let Some(resp) = rx_resp.try_pop() {
                transport.stage_response(resp.data);
            }
        }

        let staged_tx = transport.staged_response();
        rx_frame.fill(0);
        irq_flags.clear(SPI_IRQ_CS_FALLING);
        irq_flags.clear(SPI_IRQ_CS_RISING);
        io_sm.clear_fifos();
        io_sm.restart();

        let tx_fifo_ptr = io_sm.tx_fifo_ptr() as *mut u8;
        let rx_fifo_ptr = io_sm.rx_fifo_ptr() as *const u8;
        let tx_treq = io_sm.tx_treq();
        let rx_treq = io_sm.rx_treq();
        {
            let tx_transfer =
                unsafe { tx_dma.write(staged_tx.as_slice(), tx_fifo_ptr, tx_treq, false) };
            let rx_transfer = unsafe { rx_dma.read(rx_fifo_ptr, &mut rx_frame, rx_treq, false) };
            io_sm.set_enable(true);
            cs_irq.wait().await;
            irq_flags.clear(SPI_IRQ_CS_FALLING);
            cs_release_irq.wait().await;
            let _keep_alive = (&tx_transfer, &rx_transfer);
        };
        let received = rx_bytes_received(&rx_dma, &rx_frame);
        io_sm.set_enable(false);
        io_sm.clear_fifos();
        io_sm.restart();
        irq_flags.clear(SPI_IRQ_CS_RISING);

        let result = transport.finish_transaction(&rx_frame, received);

        if static_mode {
            continue;
        }

        if echo_mode {
            match result {
                TransactionResult::Complete(frame) => transport.stage_response(frame),
                TransactionResult::IdlePoll { .. } => {
                    transport.stage_response(make_response_frame(RESP_DATA_MAGIC, b""))
                }
                TransactionResult::Partial { .. } => transport.stage_response(make_response_frame(
                    RESP_COMMAND_MAGIC,
                    b"error partial spi frame",
                )),
            }
            continue;
        }

        if let Some(next) = finalize_transaction(result, bridge_config, link_active, tx).await {
            transport.stage_response(next);
        }
    }
}

struct PioSpiCsProgram<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
}

impl<'d> PioSpiCsProgram<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .side_set 1 pindirs
                .wrap_target
                wait 0 gpio 13 side 0
                irq set 1 side 1
                wait 1 gpio 13 side 1
                irq set 2 side 0
                .wrap
            "#
        );
        Self {
            loaded: common.load_program(&prg.program),
        }
    }
}

struct PioSpiMode1Program<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
}

impl<'d> PioSpiMode1Program<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .wrap_target
                jmp wait_falling
            wait_falling:
                wait 0 gpio 10
            bitloop:
                pull ifempty noblock
                out pins, 1
                wait 1 gpio 10
                in pins, 1
                push iffull noblock
                .wrap
            "#
        );
        Self {
            loaded: common.load_program(&prg.program),
        }
    }
}

fn configure_cs_sm<'d>(
    common: &mut Common<'d, PIO1>,
    cs_sm: &mut embassy_rp::pio::StateMachine<'d, PIO1, SPI_PIO_CS_SM>,
    cs: Peri<'d, PIN_13>,
    program: &PioSpiCsProgram<'d>,
) {
    let miso_pin = common.make_pio_pin(unsafe { PIN_11::steal() });
    let _cs_pin = common.make_pio_pin(cs);

    let mut cfg = PioConfig::default();
    cfg.use_program(&program.loaded, &[&miso_pin]);
    cs_sm.set_config(&cfg);
    cs_sm.set_pin_dirs(PioDirection::In, &[&miso_pin]);
}

fn configure_io_sm<'d>(
    common: &mut Common<'d, PIO1>,
    io_sm: &mut embassy_rp::pio::StateMachine<'d, PIO1, SPI_PIO_IO_SM>,
    sclk: Peri<'d, PIN_10>,
    miso: Peri<'d, PIN_11>,
    mosi: Peri<'d, PIN_12>,
    program: &PioSpiMode1Program<'d>,
) {
    let sclk_pin = common.make_pio_pin(sclk);
    let miso_pin = common.make_pio_pin(miso);
    let mosi_pin = common.make_pio_pin(mosi);

    let mut cfg = PioConfig::default();
    cfg.use_program(&program.loaded, &[]);
    cfg.set_in_pins(&[&mosi_pin]);
    cfg.set_out_pins(&[&miso_pin]);
    cfg.shift_in.auto_fill = false;
    cfg.shift_in.direction = ShiftDirection::Left;
    cfg.shift_in.threshold = 8;
    cfg.shift_out.auto_fill = false;
    cfg.shift_out.direction = ShiftDirection::Left;
    cfg.shift_out.threshold = 8;
    cfg.clock_divider = 1u8.into();
    io_sm.set_config(&cfg);
    io_sm.set_pins(Level::Low, &[&miso_pin]);
    io_sm.set_pin_dirs(PioDirection::Out, &[&miso_pin]);
    io_sm.set_pin_dirs(PioDirection::In, &[&sclk_pin, &mosi_pin]);
}

fn rx_bytes_received(rx_dma: &Channel<'_>, rx_frame: &[u8; FRAME_SIZE]) -> usize {
    let start = rx_frame.as_ptr() as usize;
    let end = rx_dma.write_addr() as usize;
    end.saturating_sub(start).min(FRAME_SIZE)
}

async fn finalize_transaction(
    result: TransactionResult,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Option<[u8; FRAME_SIZE]> {
    let frame = match result {
        TransactionResult::IdlePoll { received, preview } => {
            let payload = [
                b'R',
                received.min(255) as u8,
                preview[0],
                preview[1],
                preview[2],
                preview[3],
            ];
            return Some(make_response_frame(RESP_COMMAND_MAGIC, &payload));
        }
        TransactionResult::Partial { .. } => {
            return Some(make_response_frame(
                RESP_COMMAND_MAGIC,
                b"error partial spi frame",
            ));
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
            tx.push_overwrite(SpiFrame { data: frame });
            None
        }
        None => Some(make_response_frame(
            RESP_COMMAND_MAGIC,
            b"error invalid spi frame",
        )),
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
