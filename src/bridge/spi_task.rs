//! PIO-backed SPI slave task for framed upstream transfers.

use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, make_response_frame,
};
use embassy_executor::Spawner;
use embassy_rp::Peri;
use embassy_rp::dma::Channel;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, PIO1};
use embassy_rp::pio::{
    Common, Config as PioConfig, Direction as PioDirection, Pio, PioBatch, ShiftDirection,
};
use embassy_time::Timer;
use portable_atomic::AtomicBool;
use rp_pac::DMA;

const SPI_PIO_INITIAL_SM: usize = 0;
const SPI_PIO_CS_SM: usize = 1;
const SPI_PIO_IO_SM: usize = 2;
const SPI_TX_FIFO_PRELOAD_BYTES: usize = 16;
const SPI_TX_FIFO_PRELOAD_WORDS: usize = SPI_TX_FIFO_PRELOAD_BYTES / 4;
const SPI_TX_WORD_COUNT: usize = FRAME_SIZE.div_ceil(4);
const SPI_IRQ_ARM: usize = 7;
const SPI_IRQ_FIRST_BYTE: usize = 0;
const SPI_IRQ_CS_FALLING: usize = 1;
const SPI_IRQ_CS_RISING: usize = 2;
const SPI_TX_DMA_CHANNEL: usize = 2;
const SPI_RX_DMA_CHANNEL: usize = 3;

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
    let initial_program = PioSpiInitialProgram::new(&mut common);
    let io_program = PioSpiMode3Program::new(&mut common);
    configure_cs_sm(&mut common, &mut cs_sm, cs, &cs_program);
    configure_initial_sm(&mut common, &mut initial_sm, &initial_program);
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
    let mut tx_words = [0u32; SPI_TX_WORD_COUNT];
    let mut rx_words = [0u32; SPI_TX_WORD_COUNT];

    loop {
        if static_mode {
            transport.stage_response(static_frame);
        } else if !echo_mode {
            if let Some(resp) = rx_resp.try_pop() {
                transport.stage_response(resp.data);
            }
        }

        let staged_tx = transport.staged_response();
        pack_tx_words(&staged_tx, &mut tx_words);
        rx_frame.fill(0);
        rx_words.fill(0);
        prepare_for_next_transaction(
            &irq_flags,
            &mut initial_sm,
            &mut io_sm,
            &initial_program,
            &io_program,
        );
        preload_tx_fifo(&mut io_sm, &tx_words);

        let tx_fifo_ptr = io_sm.tx_fifo_ptr() as *mut u8;
        let rx_fifo_ptr = io_sm.rx_fifo_ptr() as *const u32;
        let tx_treq = io_sm.tx_treq();
        let rx_treq = io_sm.rx_treq();
        {
            let tx_transfer = unsafe {
                tx_dma.write(
                    &tx_words[SPI_TX_FIFO_PRELOAD_WORDS..],
                    tx_fifo_ptr as *mut u32,
                    tx_treq,
                    false,
                )
            };
            let rx_transfer = unsafe { rx_dma.read(rx_fifo_ptr, &mut rx_words, rx_treq, false) };
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
        let received = stop_transaction(&irq_flags, &mut initial_sm, &mut io_sm);
        unpack_rx_words(&rx_words, &mut rx_frame);

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

        if let Some(next) = finalize_transaction(result, tx).await {
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

struct PioSpiInitialProgram<'d> {
    loaded: embassy_rp::pio::LoadedProgram<'d, PIO1>,
}

impl<'d> PioSpiInitialProgram<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .wrap_target
                wait 1 irq 7
            bitloop:
                wait 0 gpio 10
                wait 1 gpio 10
                jmp y-- bitloop
                irq set 0
                wait 1 gpio 13
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
}

impl<'d> PioSpiMode3Program<'d> {
    fn new(common: &mut Common<'d, PIO1>) -> Self {
        let prg = pio::pio_asm!(
            r#"
                .wrap_target
                wait 1 irq 7
            wait_falling:
                wait 0 gpio 10
            bitloop:
                pull ifempty noblock
                out pins, 1
                wait 1 gpio 10
                in pins, 1
                push iffull noblock
                jmp wait_falling
                .wrap
            "#
        );
        Self { loaded: common.load_program(&prg.program) }
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
    program: &PioSpiInitialProgram<'d>,
) {
    let mosi_pin = common.make_pio_pin(unsafe { PIN_12::steal() });

    let mut cfg = PioConfig::default();
    cfg.use_program(&program.loaded, &[]);
    cfg.set_in_pins(&[&mosi_pin]);
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
    cfg.shift_out.threshold = 32;
    cfg.clock_divider = 1u8.into();
    io_sm.set_config(&cfg);
    io_sm.set_pins(Level::Low, &[&miso_pin]);
    io_sm.set_pin_dirs(PioDirection::In, &[&miso_pin]);
    io_sm.set_pin_dirs(PioDirection::In, &[&sclk_pin, &mosi_pin]);
}

fn prepare_for_next_transaction(
    irq_flags: &embassy_rp::pio::IrqFlags<'_, PIO1>,
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    initial_program: &PioSpiInitialProgram<'_>,
    program: &PioSpiMode3Program<'_>,
) {
    stop_transaction(irq_flags, initial_sm, io_sm);
    irq_flags.clear(SPI_IRQ_FIRST_BYTE);
    irq_flags.clear(SPI_IRQ_ARM);
    irq_flags.clear(SPI_IRQ_CS_FALLING);
    irq_flags.clear(SPI_IRQ_CS_RISING);
    arm_transaction_state_machines(initial_sm, io_sm, initial_program, program);
}

fn arm_transaction_state_machines(
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    initial_program: &PioSpiInitialProgram<'_>,
    program: &PioSpiMode3Program<'_>,
) {
    unsafe {
        initial_sm.set_y(7);
        initial_sm.exec_jmp(initial_program.loaded.origin);
        io_sm.exec_jmp(program.loaded.origin);
    }
}

fn stop_transaction(
    irq_flags: &embassy_rp::pio::IrqFlags<'_, PIO1>,
    initial_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_INITIAL_SM>,
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
) -> usize {
    let received = rx_bytes_received();
    abort_dma_channel(SPI_TX_DMA_CHANNEL);
    abort_dma_channel(SPI_RX_DMA_CHANNEL);
    initial_sm.set_enable(false);
    io_sm.set_enable(false);
    initial_sm.clear_fifos();
    io_sm.clear_fifos();
    initial_sm.restart();
    io_sm.restart();
    irq_flags.clear(SPI_IRQ_CS_RISING);
    received
}

fn preload_tx_fifo(
    io_sm: &mut embassy_rp::pio::StateMachine<'_, PIO1, SPI_PIO_IO_SM>,
    words: &[u32; SPI_TX_WORD_COUNT],
) {
    let tx = io_sm.tx();
    for &word in &words[..SPI_TX_FIFO_PRELOAD_WORDS] {
        tx.push(word);
    }
}

fn pack_tx_words(frame: &[u8; FRAME_SIZE], words: &mut [u32; SPI_TX_WORD_COUNT]) {
    for word in words.iter_mut() {
        *word = 0;
    }

    for (index, chunk) in frame.chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        words[index] = u32::from_be_bytes(word);
    }
}

fn rx_bytes_received() -> usize {
    let remaining = DMA
        .ch(SPI_RX_DMA_CHANNEL)
        .trans_count()
        .read()
        .min(FRAME_SIZE as u32) as usize;
    FRAME_SIZE.saturating_sub(remaining)
}

fn abort_dma_channel(channel: usize) {
    DMA.chan_abort().modify(|m| m.set_chan_abort(1 << channel));
    while DMA.ch(channel).ctrl_trig().read().busy() {}
}

fn unpack_rx_words(words: &[u32; SPI_TX_WORD_COUNT], frame: &mut [u8; FRAME_SIZE]) {
    for (index, word) in words.iter().enumerate() {
        let base = index * 4;
        if base >= FRAME_SIZE {
            break;
        }
        let bytes = word.to_le_bytes();
        for (offset, byte) in bytes.into_iter().enumerate() {
            let position = base + offset;
            if position >= FRAME_SIZE {
                break;
            }
            frame[position] = byte;
        }
    }
}

async fn finalize_transaction(
    result: TransactionResult,
    tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Option<[u8; FRAME_SIZE]> {
    match result {
        TransactionResult::IdlePoll { .. } => {
            Some(make_response_frame(RESP_DATA_MAGIC, b""))
        }
        TransactionResult::Partial { .. } => {
            Some(make_response_frame(
                RESP_COMMAND_MAGIC,
                b"error partial spi frame",
            ))
        }
        TransactionResult::Complete(frame) => {
            tx.push_overwrite(SpiFrame { data: frame });
            None
        }
    }
}
