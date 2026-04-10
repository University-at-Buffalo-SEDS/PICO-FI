//! Hardware SPI1 slave task for framed upstream transfers.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::bridge::spi_diag;
use crate::bridge::spi_pio::{PioSpiTransportState, TransactionResult};
use crate::bridge::spi_task::SpiFrame;
use crate::config::BridgeConfig;
use crate::protocol::i2c::{
    FRAME_SIZE, REQ_COMMAND_MAGIC, REQ_DATA_MAGIC, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC,
    RequestFrame, make_response_frame, parse_request_frame,
};
use core::hint::spin_loop;
use embassy_futures::yield_now;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH2, DMA_CH3, PIN_10, PIN_11, PIN_12, PIN_13, SPI1};
use embassy_rp::spi::{Config as SpiConfig, Phase, Polarity, Spi};
use embassy_rp::Peri;
use embassy_time::Timer;
use portable_atomic::AtomicBool;

const SPI_TX_PRELOAD_BYTES: usize = 8;
const SPI_IDLE_BREAK_SPINS: u32 = 32;
const SPI_WAIT_YIELD_SPINS: u32 = 2048;

pub async fn spi_poll_task(
    spi1: Peri<'static, SPI1>,
    sclk: Peri<'static, PIN_10>,
    miso: Peri<'static, PIN_11>,
    mosi: Peri<'static, PIN_12>,
    cs: Peri<'static, PIN_13>,
    _tx_dma: Peri<'static, DMA_CH2>,
    _rx_dma: Peri<'static, DMA_CH3>,
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
        drop(spi1);
        drop(sclk);
        drop(mosi);
        drop(cs);
        let mut miso = Output::new(miso, Level::High);
        loop {
            miso.set_high();
            Timer::after_secs(1).await;
        }
    }

    let mut config = SpiConfig::default();
    config.phase = Phase::CaptureOnSecondTransition;
    config.polarity = Polarity::IdleHigh;
    config.frequency = 1_000_000;
    let _spi = Spi::new_blocking(spi1, sclk, miso, mosi, config.clone());
    configure_cs_pin_for_spi();
    configure_spi1_slave_mode();
    reset_spi1_transaction_state();

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
    let mut pending_pull_response: Option<[u8; FRAME_SIZE]> = None;
    refresh_staged_response(&mut transport, static_mode, static_frame);

    loop {
        if !static_mode && !echo_mode {
            refresh_staged_response(&mut transport, static_mode, static_frame);
        }
        let staged_tx = transport.staged_response();
        rx_frame.fill(0);
        let (received, written) = spi1_slave_transfer(&staged_tx, &mut rx_frame).await;
        reset_spi1_transaction_state();

        let result = transport.finish_transaction(&rx_frame, received);
        if should_preserve_staged_response(&staged_tx, written) {
            transport.stage_response(staged_tx);
        }
        if pending_pull_response.is_some()
            && is_default_empty_data_response(&transport.staged_response())
        {
            pending_pull_response = None;
        }
        spi_diag::record_transaction(
            match result {
                TransactionResult::IdlePoll { .. } => spi_diag::idle_kind(),
                TransactionResult::Partial { .. } => spi_diag::partial_kind(),
                TransactionResult::Complete(_) => spi_diag::complete_kind(),
            },
            received,
            expected_len_from_frame(&rx_frame, received),
            &rx_frame[..received.min(8)],
            rx_frame[0],
            written,
            written.saturating_mul(8),
            staged_tx[0],
            staged_tx[1],
        );

        if static_mode {
            transport.stage_response(static_frame);
            continue;
        }

        if echo_mode {
            match result {
                TransactionResult::Complete(_) => transport.stage_response(static_frame),
                TransactionResult::IdlePoll { received, preview } => {
                    let response = make_response_frame(
                        RESP_COMMAND_MAGIC,
                        render_spi_diag("idle", received, 0, &preview).as_bytes(),
                    );
                    transport.stage_response(response)
                }
                TransactionResult::Partial {
                    received,
                    expected,
                    ..
                } => {
                    let response = make_response_frame(
                        RESP_COMMAND_MAGIC,
                        render_spi_diag("part", received, expected, &rx_frame[..8]).as_bytes(),
                    );
                    transport.stage_response(response)
                }
            }
            continue;
        }

        if let Some(next) = finalize_transaction(
            result,
            bridge_config,
            link_active,
            tx,
            rx_resp,
            &mut pending_pull_response,
        ) {
            transport.stage_response(next);
        }
        yield_now().await;
    }
}

fn refresh_staged_response(
    transport: &mut PioSpiTransportState,
    static_mode: bool,
    static_frame: [u8; FRAME_SIZE],
) {
    if static_mode {
        transport.stage_response(static_frame);
    }
}

fn configure_cs_pin_for_spi() {
    rp_pac::IO_BANK0.gpio(13).ctrl().write(|w| w.set_funcsel(1));
    rp_pac::PADS_BANK0.gpio(13).write(|w| {
        w.set_schmitt(true);
        w.set_slewfast(false);
        w.set_ie(true);
        w.set_od(false);
        w.set_pue(false);
        w.set_pde(false);
    });
}

fn configure_spi1_slave_mode() {
    let regs = rp_pac::SPI1;
    regs.cpsr().write(|w| w.set_cpsdvsr(2));
    regs.cr0().write(|w| {
        w.set_dss(0b0111);
        w.set_spo(true);
        w.set_sph(true);
        w.set_scr(0);
    });
    regs.dmacr().write(|w| {
        w.set_rxdmae(false);
        w.set_txdmae(false);
    });
    regs.cr1().write(|w| w.set_sse(false));
    regs.cr1().modify(|w| {
        w.set_ms(true);
        w.set_sod(false);
    });
    regs.cr1().modify(|w| w.set_sse(true));
}

fn reset_spi1_transaction_state() {
    let regs = rp_pac::SPI1;
    while regs.sr().read().bsy() {}
    regs.cr1().modify(|w| w.set_sse(false));
    while regs.sr().read().rne() {
        let _ = regs.dr().read().data();
    }
    regs.icr().write(|w| {
        w.set_roric(true);
        w.set_rtic(true);
    });
    regs.cr1().modify(|w| w.set_sse(true));
}

async fn spi1_slave_transfer(tx_frame: &[u8; FRAME_SIZE], rx_frame: &mut [u8; FRAME_SIZE]) -> (usize, usize) {
    let status_before = read_spi1_status();
    let (received, written) = spi1_manual_transfer(tx_frame, rx_frame).await;
    let status_after = read_spi1_status();
    let ok = received > 0 || written > 0;
    spi_diag::record_transfer_status(status_before, status_after, ok);
    if !ok {
        return (0, 0);
    }
    (received, written)
}

fn read_spi1_status() -> u8 {
    let sr = rp_pac::SPI1.sr().read();
    (sr.tfe() as u8)
        | ((sr.tnf() as u8) << 1)
        | ((sr.rne() as u8) << 2)
        | ((sr.rff() as u8) << 3)
        | ((sr.bsy() as u8) << 4)
}

async fn spi1_manual_transfer(tx_frame: &[u8; FRAME_SIZE], rx_frame: &mut [u8; FRAME_SIZE]) -> (usize, usize) {
    let regs = rp_pac::SPI1;

    let mut tx_index = 0usize;
    while tx_index < SPI_TX_PRELOAD_BYTES && tx_index < FRAME_SIZE && regs.sr().read().tnf() {
        regs.dr().write(|w| w.set_data(tx_frame[tx_index] as u16));
        tx_index += 1;
    }

    let mut wait_spins = 0u32;
    while !cs_asserted() {
        wait_spins = wait_spins.saturating_add(1);
        if wait_spins >= SPI_WAIT_YIELD_SPINS {
            wait_spins = 0;
            yield_now().await;
        }
        spin_loop();
    }

    let mut rx_index = 0usize;
    let mut idle_spins = 0u32;
    loop {
        let sr = regs.sr().read();
        let cs_low = cs_asserted();

        if sr.rne() {
            let byte = regs.dr().read().data() as u8;
            if rx_index < FRAME_SIZE {
                rx_frame[rx_index] = byte;
                rx_index += 1;
            }
            idle_spins = 0;
        } else if !cs_low && !sr.bsy() {
            idle_spins = idle_spins.saturating_add(1);
            if idle_spins >= SPI_IDLE_BREAK_SPINS {
                break;
            }
        } else {
            idle_spins = 0;
        }

        while tx_index < FRAME_SIZE && regs.sr().read().tnf() {
            regs.dr().write(|w| w.set_data(tx_frame[tx_index] as u16));
            tx_index += 1;
            if tx_index >= SPI_TX_PRELOAD_BYTES && !cs_low && !regs.sr().read().rne() {
                break;
            }
        }

        if !cs_low && !regs.sr().read().bsy() && !regs.sr().read().rne() && tx_index >= FRAME_SIZE {
            break;
        }

        spin_loop();
    }

    while regs.sr().read().bsy() {
        spin_loop();
    }

    (rx_index, rx_index.min(tx_index))
}

fn cs_asserted() -> bool {
    (rp_pac::SIO.gpio_in(0).read() & (1 << 13)) == 0
}

fn is_default_empty_data_response(frame: &[u8; FRAME_SIZE]) -> bool {
    frame[0] == RESP_DATA_MAGIC && frame[1] == 0
}

fn should_preserve_staged_response(frame: &[u8; FRAME_SIZE], written: usize) -> bool {
    match frame[0] {
        RESP_COMMAND_MAGIC => written < frame[1] as usize + 2,
        RESP_DATA_MAGIC if frame[1] != 0 => written < frame[1] as usize + 2,
        _ => false,
    }
}

fn render_spi_diag(kind: &str, received: usize, expected: usize, preview: &[u8]) -> heapless::String<96> {
    let mut out = heapless::String::<96>::new();
    let _ = core::fmt::write(&mut out, format_args!("{kind} r={received} e={expected}"));
    for byte in preview.iter().take(8) {
        let _ = core::fmt::write(&mut out, format_args!(" {:02x}", byte));
    }
    out
}

fn expected_len_from_frame(frame: &[u8; FRAME_SIZE], received: usize) -> usize {
    if received >= 2 {
        (frame[1] as usize + 2).min(FRAME_SIZE)
    } else {
        0
    }
}

fn finalize_transaction(
    result: TransactionResult,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: &'static OverwriteQueue<SpiFrame, 8>,
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
    pending_pull_response: &mut Option<[u8; FRAME_SIZE]>,
) -> Option<[u8; FRAME_SIZE]> {
    match result {
        TransactionResult::IdlePoll { .. } => Some(make_response_frame(RESP_DATA_MAGIC, b"")),
        TransactionResult::Partial {
            received,
            expected,
            frame,
        } => {
            if let Some(line) = extract_ascii_command(&frame) {
                if line == "/pull" {
                    return Some(pull_queued_response(rx_resp, pending_pull_response));
                }
                if line.starts_with('/') {
                    let response = render_local_bridge_command(bridge_config, link_active, line);
                    return Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()));
                }
            }
            Some(make_response_frame(
                RESP_COMMAND_MAGIC,
                render_spi_diag("part", received, expected, &frame[..8]).as_bytes(),
            ))
        }
        TransactionResult::Complete(frame) => match parse_request_frame(&frame) {
            Some(RequestFrame::Command(payload)) => {
                let line = trim_ascii_line(payload);
                if line == "/pull" {
                    Some(pull_queued_response(rx_resp, pending_pull_response))
                } else if line.starts_with('/') {
                    let response = render_local_bridge_command(bridge_config, link_active, line);
                    Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()))
                } else if payload.is_empty() {
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                } else if is_strict_forward_frame(&frame) {
                    tx.push_overwrite(SpiFrame { data: frame });
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                } else {
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                }
            }
            Some(RequestFrame::Data(payload)) => {
                if let Some(line) = extract_ascii_command(&frame) {
                    if line == "/pull" {
                        return Some(pull_queued_response(rx_resp, pending_pull_response));
                    }
                    let response = render_local_bridge_command(bridge_config, link_active, line);
                    return Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()));
                }
                if payload.is_empty() {
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                } else if is_strict_forward_frame(&frame) {
                    tx.push_overwrite(SpiFrame { data: frame });
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                } else {
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                }
            }
            None => {
                if let Some(line) = extract_ascii_command(&frame) {
                    if line.starts_with('/') {
                        let response = render_local_bridge_command(bridge_config, link_active, line);
                        return Some(make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes()));
                    }
                }
                if frame[..8].iter().any(|&byte| byte != 0) {
                    Some(make_response_frame(
                        RESP_COMMAND_MAGIC,
                        render_spi_diag("none", 0, 0, &frame[..8]).as_bytes(),
                    ))
                } else {
                    Some(make_response_frame(RESP_DATA_MAGIC, b""))
                }
            }
        },
    }
}

fn pull_queued_response(
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
    pending_pull_response: &mut Option<[u8; FRAME_SIZE]>,
) -> [u8; FRAME_SIZE] {
    if let Some(resp) = pending_pull_response {
        return *resp;
    }
    if let Some(resp) = rx_resp.try_pop() {
        *pending_pull_response = Some(resp.data);
        return resp.data;
    }
    make_response_frame(RESP_DATA_MAGIC, b"")
}

fn is_strict_forward_frame(frame: &[u8; FRAME_SIZE]) -> bool {
    if !matches!(frame[0], REQ_DATA_MAGIC | REQ_COMMAND_MAGIC) {
        return false;
    }
    let len = frame[1] as usize;
    if len > FRAME_SIZE - 2 {
        return false;
    }
    frame[2 + len..].iter().all(|&byte| byte == 0)
}

fn extract_ascii_command(frame: &[u8; FRAME_SIZE]) -> Option<&str> {
    let scan = &frame[..FRAME_SIZE.min(96)];
    let mut start = 0usize;
    while let Some(offset) = scan[start..].iter().position(|&byte| byte == b'/') {
        let slash = start + offset;
        let tail = &scan[slash..];
        let end = tail
            .iter()
            .position(|&byte| byte == 0 || byte == b'\n' || byte == b'\r')
            .unwrap_or(tail.len());
        if let Ok(candidate) = core::str::from_utf8(&tail[..end]) {
            if candidate
                .bytes()
                .all(|byte| byte == b'/' || (32..=126).contains(&byte))
            {
                return Some(candidate);
            }
        }
        start = slash + 1;
    }
    None
}
