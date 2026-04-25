//! UART upstream bridge implementation using framed request/response packets.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteBytePacketRing;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{
    connect_with_timeout, exchange_link_handshake, read_bridge_frame, write_bridge_frame,
};
use crate::protocol::i2c::{
    FRAME_HEADER_SIZE, REQ_COMMAND_MAGIC, REQ_DATA_MAGIC, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC,
};
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use portable_atomic::{AtomicBool, Ordering};

const UART_EGRESS_RING_PACKETS: usize = 8;
const UART_EGRESS_PACKET_BYTES: usize = UART_PAYLOAD_MAX + UART_FRAME_HEADER_SIZE;
const UART_EGRESS_CHUNK_BYTES: usize = 256;
const UART_PAYLOAD_MAX: usize = 4096;
const UART_FRAME_HEADER_SIZE: usize = FRAME_HEADER_SIZE;
const UART_RETRY_DELAY_MS: u64 = 10;
const UART_FLUSH_BATCH_CHUNKS: usize = 4;
const UART_PRECONNECT_NET_SLICE_MS: u64 = 50;
const UART_PRECONNECT_UART_SLICE_MS: u64 = 1;

type UartEgressRing = OverwriteBytePacketRing<UART_EGRESS_RING_PACKETS, UART_EGRESS_PACKET_BYTES>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum UartFrameKind {
    Data,
    Command,
}

struct UartFrame {
    kind: UartFrameKind,
    len: usize,
    payload: [u8; UART_PAYLOAD_MAX],
}

impl UartFrame {
    const fn new() -> Self {
        Self {
            kind: UartFrameKind::Data,
            len: 0,
            payload: [0; UART_PAYLOAD_MAX],
        }
    }

    fn payload(&self) -> &[u8] {
        &self.payload[..self.len]
    }
}

pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    Timer::after_millis(runtime.startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(3)));

        runtime.link_active.store(false, Ordering::Relaxed);
        {
            let (uart_tx, uart_rx) = uart.split_ref();
            let mut uart_frame = UartFrame::new();
            let mut egress_ring = UartEgressRing::new();

            loop {
                if connect_with_timeout(&mut socket, remote, port, UART_PRECONNECT_NET_SLICE_MS)
                    .await
                    .is_ok()
                {
                    break;
                }

                service_preconnect_uart(
                    uart_tx,
                    uart_rx,
                    &mut uart_frame,
                    bridge_config,
                    runtime.link_active,
                    &mut egress_ring,
                )
                .await?;

                if !egress_ring.is_empty() {
                    flush_uart_egress(uart_tx, &mut egress_ring).await;
                }

                yield_now().await;

                if runtime.connect_timeout_ms <= UART_PRECONNECT_NET_SLICE_MS {
                    Timer::after_millis(runtime.reconnect_delay_ms).await;
                }
            }
        }
        socket.set_timeout(Some(Duration::from_millis(runtime.session_timeout_ms)));
        if exchange_link_handshake(
            &mut socket,
            true,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
        .await
        .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(uart, &mut socket, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}

pub async fn run_server(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(3)));

        runtime.link_active.store(false, Ordering::Relaxed);
        {
            let (uart_tx, uart_rx) = uart.split_ref();
            let mut uart_frame = UartFrame::new();
            let mut egress_ring = UartEgressRing::new();

            loop {
                match select(
                    socket.accept(port),
                    Timer::after_millis(UART_PRECONNECT_NET_SLICE_MS),
                )
                .await
                {
                    Either::First(Ok(())) => break,
                    Either::First(Err(_)) => {
                        Timer::after_millis(runtime.reconnect_delay_ms).await;
                    }
                    Either::Second(()) => {}
                }

                service_preconnect_uart(
                    uart_tx,
                    uart_rx,
                    &mut uart_frame,
                    bridge_config,
                    runtime.link_active,
                    &mut egress_ring,
                )
                .await?;

                if !egress_ring.is_empty() {
                    flush_uart_egress(uart_tx, &mut egress_ring).await;
                }

                yield_now().await;
            }
        }
        socket.set_timeout(Some(Duration::from_millis(runtime.session_timeout_ms)));
        if exchange_link_handshake(
            &mut socket,
            false,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
        .await
        .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            runtime.link_active.store(false, Ordering::Relaxed);
            continue;
        }
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(uart, &mut socket, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

async fn session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
    let mut uart_frame = UartFrame::new();
    let mut net_buf = [0u8; UART_PAYLOAD_MAX];
    let mut tx_chunk = [0u8; UART_EGRESS_CHUNK_BYTES];
    let mut egress_ring = UartEgressRing::new();
    let mut tx_chunk_len = 0usize;
    let mut tx_chunk_pos = 0usize;
    let mut consecutive_tx_chunks = 0usize;
    let (uart_tx, uart_rx) = uart.split_ref();

    loop {
        if tx_chunk_pos >= tx_chunk_len && !egress_ring.is_empty() {
            tx_chunk_len = egress_ring.peek_into(&mut tx_chunk);
            tx_chunk_pos = 0;
        }

        if tx_chunk_pos < tx_chunk_len {
            let written =
                write_uart_chunk_lossy(uart_tx, &tx_chunk[tx_chunk_pos..tx_chunk_len]).await;
            egress_ring.consume(written);
            tx_chunk_pos = (tx_chunk_pos + written).min(tx_chunk_len);
            consecutive_tx_chunks += 1;
            if tx_chunk_pos >= tx_chunk_len {
                tx_chunk_pos = 0;
                tx_chunk_len = 0;
            }
            if consecutive_tx_chunks >= UART_FLUSH_BATCH_CHUNKS {
                consecutive_tx_chunks = 0;
                yield_now().await;
            }
            continue;
        } else {
            consecutive_tx_chunks = 0;
            match select(
                read_bridge_frame(socket, &mut net_buf),
                read_uart_frame_lossy(uart_rx, &mut uart_frame),
            )
            .await
            {
                Either::First(Ok(net_n)) => {
                    push_uart_frame(&mut egress_ring, UartFrameKind::Data, &net_buf[..net_n]);
                }
                Either::First(Err(_)) => return Err(()),
                Either::Second(()) => {
                    handle_uart_request(
                        &uart_frame,
                        Some(socket),
                        bridge_config,
                        link_active,
                        &mut egress_ring,
                    )
                    .await?;
                }
            }
        }

        yield_now().await;
    }
}

async fn handle_uart_request(
    frame: &UartFrame,
    socket: Option<&mut TcpSocket<'_>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    egress_ring: &mut UartEgressRing,
) -> Result<(), ()> {
    match frame.kind {
        UartFrameKind::Data => {
            let payload = frame.payload();
            if let Some(socket) = socket {
                if !payload.is_empty() {
                    write_bridge_frame(socket, payload).await?;
                }
            }
            Ok(())
        }
        UartFrameKind::Command => {
            let payload = frame.payload();
            let line = trim_ascii_line(payload);
            let response = render_local_bridge_command(bridge_config, link_active, line);
            push_uart_frame(egress_ring, UartFrameKind::Command, response.as_bytes());
            Ok(())
        }
    }
}

fn push_uart_frame(egress_ring: &mut UartEgressRing, kind: UartFrameKind, payload: &[u8]) {
    let len = payload.len().min(UART_PAYLOAD_MAX);
    let mut header = [0u8; UART_FRAME_HEADER_SIZE];
    match kind {
        UartFrameKind::Data => {
            header[0] = REQ_DATA_MAGIC;
            header[1] = RESP_DATA_MAGIC;
        }
        UartFrameKind::Command => {
            header[0] = REQ_COMMAND_MAGIC;
            header[1] = RESP_COMMAND_MAGIC;
        }
    }
    header[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    egress_ring.push_overwrite_slices(&[&header, &payload[..len]]);
}

async fn flush_uart_egress(uart_tx: &mut impl Write, egress_ring: &mut UartEgressRing) {
    let mut tx_chunk = [0u8; UART_EGRESS_CHUNK_BYTES];
    let mut flushed_chunks = 0usize;
    while !egress_ring.is_empty() {
        let chunk_len = egress_ring.peek_into(&mut tx_chunk);
        if chunk_len == 0 {
            break;
        }
        match uart_tx.write(&tx_chunk[..chunk_len]).await {
            Ok(0) | Err(_) => {
                egress_ring.clear();
                break;
            }
            Ok(written) if written < chunk_len => {
                egress_ring.consume(written);
                break;
            }
            Ok(_) => {
                egress_ring.consume(chunk_len);
                let _ = uart_tx.flush().await;
            }
        }
        flushed_chunks += 1;
        if flushed_chunks >= UART_FLUSH_BATCH_CHUNKS {
            flushed_chunks = 0;
            yield_now().await;
        }
    }
}

async fn service_preconnect_uart(
    uart_tx: &mut impl Write,
    uart_rx: &mut impl Read,
    uart_frame: &mut UartFrame,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    egress_ring: &mut UartEgressRing,
) -> Result<(), ()> {
    match select(
        read_uart_frame_lossy(uart_rx, uart_frame),
        Timer::after_millis(UART_PRECONNECT_UART_SLICE_MS),
    )
    .await
    {
        Either::First(()) => {
            handle_uart_request(uart_frame, None, bridge_config, link_active, egress_ring).await?;
            if !egress_ring.is_empty() {
                flush_uart_egress(uart_tx, egress_ring).await;
            }
            Ok(())
        }
        Either::Second(()) => Ok(()),
    }
}

async fn read_uart_frame_lossy(uart_rx: &mut impl Read, frame: &mut UartFrame) {
    loop {
        if read_uart_request_frame(uart_rx, frame).await.is_ok() {
            return;
        }
        Timer::after_millis(UART_RETRY_DELAY_MS).await;
    }
}

async fn write_uart_chunk_lossy(uart_tx: &mut impl Write, chunk: &[u8]) -> usize {
    loop {
        match uart_tx.write(chunk).await {
            Ok(written) if written != 0 => return written,
            Ok(_) | Err(_) => {
                Timer::after_millis(UART_RETRY_DELAY_MS).await;
            }
        }
    }
}

async fn read_uart_request_frame(uart_rx: &mut impl Read, frame: &mut UartFrame) -> Result<(), ()> {
    let mut byte = [0u8; 1];

    loop {
        uart_rx.read_exact(&mut byte).await.map_err(|_| ())?;
        if matches!(byte[0], REQ_DATA_MAGIC | REQ_COMMAND_MAGIC) {
            break;
        }
    }

    let first = byte[0];
    uart_rx.read_exact(&mut byte).await.map_err(|_| ())?;
    let kind = match (first, byte[0]) {
        (REQ_DATA_MAGIC, RESP_DATA_MAGIC) => UartFrameKind::Data,
        (REQ_COMMAND_MAGIC, RESP_COMMAND_MAGIC) => UartFrameKind::Command,
        _ => return Err(()),
    };

    let mut len_bytes = [0u8; 2];
    uart_rx.read_exact(&mut len_bytes).await.map_err(|_| ())?;
    let len = u16::from_le_bytes(len_bytes) as usize;
    if len > UART_PAYLOAD_MAX {
        return Err(());
    }

    if len != 0 {
        uart_rx
            .read_exact(&mut frame.payload[..len])
            .await
            .map_err(|_| ())?;
    }
    frame.kind = kind;
    frame.len = len;

    Ok(())
}
