//! PIO-backed SPI slave task for framed upstream transfers.

use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_rp::Peri;
use embassy_rp::dma::Channel;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{
    Common, Config as PioConfig, Direction as PioDirection, Pio, PioBatch, ShiftDirection,
};
use embassy_time::Timer;
use portable_atomic::AtomicBool;
use pio::{InstructionOperands, MovDestination, MovOperation, MovSource, WaitSource};
use rp_pac::DMA;
use core::hint::spin_loop;

const SPI_PIO_INITIAL_SM: usize = 0;
const SPI_PIO_CS_SM: usize = 1;
const SPI_PIO_IO_SM: usize = 2;
const SPI_IRQ_ARM: usize = 7;
const SPI_IRQ_FIRST_BYTE: usize = 0;
const SPI_IRQ_CS_FALLING: usize = 1;
const SPI_IRQ_CS_RISING: usize = 2;
const SPI_TX_DMA_CHANNEL: usize = 2;
const SPI_RX_DMA_CHANNEL: usize = 3;
const SPI_LOCAL_RESPONSE_WAIT_MS: u64 = 5;
const SPI_COMMAND_RESPONSE_WAIT_MS: u64 = 100;
const SPI_DMA_SETTLE_SPINS: usize = 4096;

#[derive(Clone, Copy, Default)]
struct TransactionStats {
    num_bytes_read: usize,
    num_bytes_written: usize,
    num_bits_transacted: usize,
    first_byte: Option<u8>,
}

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
    _link_active: &AtomicBool,
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
    let mut initial_sm = pio.sm0;
    let mut cs_sm = pio.sm1;
    let mut io_sm = pio.sm2;

    let cs_program = PioSpiCsProgram::new(&mut common);
    let io_program = PioSpiMode3Program::new(&mut common);
    configure_cs_sm(&mut common, &mut cs_sm, cs, &cs_program);
    configure_initial_sm(&mut common, &mut initial_sm, &io_program);
    configure_io_sm(&mut common, &mut io_sm, sclk, miso, mosi, &io_program);

    irq_flags.clear_all(0xff);
    cs_sm.set_enable(true);
    initial_sm.set_enable(false);
    io_sm.set_enable(false);
    initial_sm.clear_fifos();
    initial_sm.restart();
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
        prepare_for_next_transaction(
            &irq_flags,
            &mut initial_sm,
            &mut io_sm,
            &io_program,
        );

        let tx_fifo_ptr = io_sm.tx_fifo_ptr() as *mut u8;
        let rx_fifo_ptr = io_sm.rx_fifo_ptr() as *const u8;
        let tx_treq = io_sm.tx_treq();
        let rx_treq = io_sm.rx_treq();
        {
            let tx_transfer = unsafe { tx_dma.write(&staged_tx, tx_fifo_ptr, tx_treq, false) };
            let rx_transfer = unsafe { rx_dma.read(rx_fifo_ptr, &mut rx_frame, rx_treq, false) };
            let mut batch = PioBatch::new();
            batch.restart(&mut initial_sm);
            batch.restart(&mut io_sm);
            batch.set_enable(&mut initial_sm, true);
            batch.set_enable(&mut io_sm, true);
            batch.execute();
            cs_irq.wait().await;
            irq_flags.clear(SPI_IRQ_CS_FALLING);
            cs_release_irq.wait().await;
            let _keep_alive = (&tx_transfer, &rx_transfer);
        }
        let tx_len = staged_tx[1] as usize + 2;
        let stats = stop_transaction(&irq_flags, &mut initial_sm, &mut io_sm, tx_len);
        let received = stats.num_bytes_read.min(FRAME_SIZE);
        apply_initial_byte(&mut rx_frame, received, stats.first_byte);

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
                TransactionResult::Partial { .. } => {
                    transport.stage_response(make_response_frame(RESP_DATA_MAGIC, b""))
                }
            }
            continue;
        }

        if let Some(next) = finalize_transaction(result, tx, rx_resp).await {
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
                irq set 7 side 1
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

struct PioSpiMode3Program<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
    initial_check: u8,
}

impl<'d> PioSpiMode3Program<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
            wait_falling:
            bitloop:
                wait 0 gpio 10
            initial_loop:
                pull ifempty noblock
                out pins, 1
                wait 1 gpio 10
                in pins, 1
                push iffull noblock
                .wrap
                .wrap_target
                jmp wait_falling
            public initial_check:
                jmp y-- wait_falling
                irq set 0
                jmp wait_falling
            "#
        );
        let loaded = common.load_program(&prg.program);
        Self {
            loaded,
            initial_check: prg.public_defines.initial_check as u8,
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

fn configure_initial_sm<'d>(
    common: &mut Common<'d, PIO1>,
    initial_sm: &mut embassy_rp::pio::StateMachine<'d, PIO1, SPI_PIO_INITIAL_SM>,
    program: &PioSpiMode3Program<'d>,
) {
    let mosi_pin = common.make_pio_pin(unsafe { PIN_12::steal() });

    let mut cfg = PioConfig::default();
    cfg.use_program(&program.loaded, &[]);
    let mut exec = cfg.get_exec();
    exec.wrap_bottom = program.loaded.origin + program.initial_check;
    exec.wrap_top = program.loaded.wrap.source;
    unsafe { cfg.set_exec(exec) };
    cfg.set_in_pins(&[&mosi_pin]);
    cfg.shift_in.auto_fill = false;
    cfg.shift_in.direction = ShiftDirection::Left;
    cfg.shift_in.threshold = 8;
    initial_sm.set_config(&cfg);
    initial_sm.set_pin_dirs(PioDirection::In, &[&mosi_pin]);
}

fn configure_io_sm<'d>(
    common: &mut Common<'d, PIO1>,
    io_sm: &mut embassy_rp::pio::StateMachine<'d, PIO1, SPI_PIO_IO_SM>,
    sclk: Peri<'d, PIN_10>,
    miso: Peri<'d, PIN_11>,
    mosi: Peri<'d, PIN_12>,
    program: &PioSpiMode3Program<'d>,
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
    io_sm.set_pin_dirs(PioDirection::In, &[&miso_pin]);
    io_sm.set_pin_dirs(PioDirection::In, &[&sclk_pin, &mosi_pin]);
    prime_default_write_value(io_sm, 0);
}

fn prepare_for_next_transaction(
    irq_flags: &embassy_rp::pio::IrqFlags<'_, PIO1>,
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    program: &PioSpiMode3Program<'_>,
) {
    let _ = stop_transaction(irq_flags, initial_sm, io_sm, 0);
    irq_flags.clear(SPI_IRQ_FIRST_BYTE);
    irq_flags.clear(SPI_IRQ_ARM);
    irq_flags.clear(SPI_IRQ_CS_FALLING);
    irq_flags.clear(SPI_IRQ_CS_RISING);
    arm_transaction_state_machines(initial_sm, io_sm, program);
}

fn arm_transaction_state_machines(
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    program: &PioSpiMode3Program<'_>,
) {
    let wait_irq = InstructionOperands::WAIT {
        polarity: 1,
        source: WaitSource::IRQ,
        index: SPI_IRQ_ARM as u8,
        relative: false,
    }
    .encode();
    unsafe {
        initial_sm.set_y(7);
        initial_sm.exec_jmp(program.loaded.origin);
        io_sm.exec_jmp(program.loaded.origin);
        initial_sm.exec_instr(wait_irq);
        io_sm.exec_instr(wait_irq);
    }
}

fn stop_transaction(
    irq_flags: &embassy_rp::pio::IrqFlags<'_, PIO1>,
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    tx_len: usize,
) -> TransactionStats {
    wait_for_dma_settle(SPI_TX_DMA_CHANNEL);
    wait_for_dma_settle(SPI_RX_DMA_CHANNEL);
    let left_in_write_queue = tx_fifo_level();
    let bits_remaining = unsafe { initial_sm.get_y() as usize };
    let first_byte = initial_sm.rx().try_pull().map(|value| value as u8);
    let read_remaining = DMA
        .ch(SPI_RX_DMA_CHANNEL)
        .trans_count()
        .read()
        .min(FRAME_SIZE as u32) as usize;
    let write_remaining = DMA
        .ch(SPI_TX_DMA_CHANNEL)
        .trans_count()
        .read()
        .min(FRAME_SIZE as u32) as usize;
    abort_dma_channel(SPI_TX_DMA_CHANNEL);
    abort_dma_channel(SPI_RX_DMA_CHANNEL);
    initial_sm.set_enable(false);
    io_sm.set_enable(false);
    initial_sm.clear_fifos();
    io_sm.clear_fifos();
    initial_sm.restart();
    io_sm.restart();
    irq_flags.clear(SPI_IRQ_CS_RISING);
    if let Some(byte) = first_byte {
        let _ = byte;
    }
    TransactionStats {
        num_bytes_read: FRAME_SIZE.saturating_sub(read_remaining),
        num_bytes_written: tx_len.saturating_sub(write_remaining).saturating_sub(left_in_write_queue),
        num_bits_transacted: ((0usize.wrapping_sub(bits_remaining)) + 7),
        first_byte,
    }
}

fn abort_dma_channel(channel: usize) {
    DMA.chan_abort().modify(|m| m.set_chan_abort(1 << channel));
    while DMA.ch(channel).ctrl_trig().read().busy() {}
}

fn tx_fifo_level() -> usize {
    let flevel = rp_pac::PIO1.flevel().read();
    match SPI_PIO_IO_SM {
        0 => flevel.tx0() as usize,
        1 => flevel.tx1() as usize,
        2 => flevel.tx2() as usize,
        3 => flevel.tx3() as usize,
        _ => 0,
    }
}

fn wait_for_dma_settle(channel: usize) {
    for _ in 0..SPI_DMA_SETTLE_SPINS {
        if !DMA.ch(channel).ctrl_trig().read().busy() {
            break;
        }
        spin_loop();
    }
}

fn apply_initial_byte(rx_frame: &mut [u8; FRAME_SIZE], received: usize, first_byte: Option<u8>) {
    let Some(first_byte) = first_byte else {
        return;
    };
    let received = received.min(FRAME_SIZE);
    if received == 0 {
        rx_frame[0] = first_byte;
        return;
    }
    if rx_frame[0] == first_byte {
        return;
    }
    if rx_frame[0] == 0 {
        let shift_len = received.min(FRAME_SIZE - 1);
        rx_frame.copy_within(0..shift_len, 1);
        rx_frame[0] = first_byte;
        return;
    }
    rx_frame[0] = first_byte;
}

async fn finalize_transaction(
    result: TransactionResult,
    tx: &'static OverwriteQueue<SpiFrame, 8>,
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
) -> Option<[u8; FRAME_SIZE]> {
    match result {
        TransactionResult::IdlePoll { .. } => {
            Some(make_response_frame(RESP_DATA_MAGIC, b""))
        }
        TransactionResult::Partial { .. } => {
            Some(make_response_frame(RESP_DATA_MAGIC, b""))
        }
        TransactionResult::Complete(frame) => {
            tx.push_overwrite(SpiFrame { data: frame });
            let wait_ms = match parse_request_frame(&frame) {
                Some(RequestFrame::Command(_)) => SPI_COMMAND_RESPONSE_WAIT_MS,
                _ => SPI_LOCAL_RESPONSE_WAIT_MS,
            };
            if let Some(response) = wait_for_local_response(rx_resp, wait_ms).await {
                Some(response.data)
            } else {
                None
            }
        }
    }
}

fn prime_default_write_value(
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    value: u8,
) {
    let pull = InstructionOperands::PULL {
        if_empty: false,
        block: true,
    }
    .encode();
    let mov = InstructionOperands::MOV {
        destination: MovDestination::X,
        op: MovOperation::None,
        source: MovSource::OSR,
    }
    .encode();
    io_sm.tx().push((value as u32) << 24);
    unsafe {
        io_sm.exec_instr(pull);
        io_sm.exec_instr(mov);
    }
}

async fn wait_for_local_response(
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
    wait_ms: u64,
) -> Option<SpiFrame> {
    if let Some(frame) = rx_resp.try_pop() {
        return Some(frame);
    }
    match select(rx_resp.pop(), Timer::after_millis(wait_ms)).await {
        Either::First(frame) => Some(frame),
        Either::Second(_) => None,
    }
}
