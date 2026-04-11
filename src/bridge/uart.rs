//! UART upstream bridge implementation using framed request/response packets.

use crate::bridge::commands::{render_local_bridge_command, signal_led_activity, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteByteRing;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::i2c::{
    FRAME_SIZE, RequestFrame, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, make_response_frame,
    parse_request_frame,
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

const UART_EGRESS_RING_BYTES: usize = 4096;
const UART_EGRESS_CHUNK_BYTES: usize = 256;
const UART_RETRY_DELAY_MS: u64 = 10;

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
            let mut uart_frame = [0u8; FRAME_SIZE];
            let mut egress_ring = OverwriteByteRing::<UART_EGRESS_RING_BYTES>::new();

            loop {
                match select(
                    uart_rx.read_exact(&mut uart_frame),
                    connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms),
                )
                .await
                {
                    Either::First(Ok(())) => {
                        handle_uart_request(
                            &uart_frame,
                            None,
                            bridge_config,
                            runtime.link_active,
                            &mut egress_ring,
                        )
                        .await?;
                        flush_uart_egress(uart_tx, &mut egress_ring).await;
                    }
                    Either::First(Err(_)) => {
                        Timer::after_millis(UART_RETRY_DELAY_MS).await;
                    }
                    Either::Second(Ok(())) => break,
                    Either::Second(Err(_)) => {
                        Timer::after_millis(runtime.reconnect_delay_ms).await;
                    }
                }
            }
        }
        socket.set_timeout(None);
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
            let mut uart_frame = [0u8; FRAME_SIZE];
            let mut egress_ring = OverwriteByteRing::<UART_EGRESS_RING_BYTES>::new();

            loop {
                match select(uart_rx.read_exact(&mut uart_frame), socket.accept(port)).await {
                    Either::First(Ok(())) => {
                        handle_uart_request(
                            &uart_frame,
                            None,
                            bridge_config,
                            runtime.link_active,
                            &mut egress_ring,
                        )
                        .await?;
                        flush_uart_egress(uart_tx, &mut egress_ring).await;
                    }
                    Either::First(Err(_)) => {
                        Timer::after_millis(UART_RETRY_DELAY_MS).await;
                    }
                    Either::Second(Ok(())) => break,
                    Either::Second(Err(_)) => {
                        Timer::after_millis(runtime.reconnect_delay_ms).await;
                    }
                }
            }
        }
        socket.set_timeout(None);
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
    let mut uart_frame = [0u8; FRAME_SIZE];
    let mut net_buf = [0u8; 256];
    let mut tx_chunk = [0u8; UART_EGRESS_CHUNK_BYTES];
    let mut egress_ring = OverwriteByteRing::<UART_EGRESS_RING_BYTES>::new();
    let mut tx_chunk_len = 0usize;
    let mut tx_chunk_pos = 0usize;
    let (uart_tx, uart_rx) = uart.split_ref();

    loop {
        if tx_chunk_pos >= tx_chunk_len && !egress_ring.is_empty() {
            tx_chunk_len = egress_ring.pop_into(&mut tx_chunk);
            tx_chunk_pos = 0;
        }

        if tx_chunk_pos < tx_chunk_len {
            match select(
                socket.read(&mut net_buf),
                write_uart_chunk_lossy(uart_tx, &tx_chunk[tx_chunk_pos..tx_chunk_len]),
            )
            .await
            {
                Either::First(Ok(net_n)) => {
                    if net_n == 0 {
                        return Ok(());
                    }
                    egress_ring.push_overwrite_slice(
                        &make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]),
                    );
                }
                Either::First(Err(_)) => return Err(()),
                Either::Second(written) => {
                    tx_chunk_pos = (tx_chunk_pos + written).min(tx_chunk_len);
                    if tx_chunk_pos >= tx_chunk_len {
                        tx_chunk_pos = 0;
                        tx_chunk_len = 0;
                    }
                }
            }
        } else {
            match select(
                socket.read(&mut net_buf),
                read_uart_frame_lossy(uart_rx, &mut uart_frame),
            )
            .await
            {
                Either::First(Ok(net_n)) => {
                    if net_n == 0 {
                        return Ok(());
                    }
                    egress_ring.push_overwrite_slice(
                        &make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]),
                    );
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

        if tx_chunk_pos >= tx_chunk_len && egress_ring.is_empty() {
            yield_now().await;
        }
    }
}

async fn handle_uart_request(
    frame: &[u8; FRAME_SIZE],
    socket: Option<&mut TcpSocket<'_>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    egress_ring: &mut OverwriteByteRing<UART_EGRESS_RING_BYTES>,
) -> Result<(), ()> {
    match parse_request_frame(frame) {
        Some(RequestFrame::Data(payload)) => {
            if let Some(socket) = socket {
                if !payload.is_empty() {
                    signal_led_activity();
                    write_socket(socket, payload).await?;
                    egress_ring.push_overwrite_slice(&make_response_frame(RESP_DATA_MAGIC, b""));
                }
            } else {
                egress_ring.push_overwrite_slice(&make_response_frame(RESP_DATA_MAGIC, b""));
            }
            Ok(())
        }
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response = render_local_bridge_command(bridge_config, link_active, line);
            egress_ring.push_overwrite_slice(&make_response_frame(
                RESP_COMMAND_MAGIC,
                response.as_bytes(),
            ));
            Ok(())
        }
        None => {
            egress_ring.push_overwrite_slice(&make_response_frame(
                RESP_COMMAND_MAGIC,
                b"error invalid uart frame",
            ));
            Ok(())
        }
    }
}

async fn flush_uart_egress(
    uart_tx: &mut impl Write,
    egress_ring: &mut OverwriteByteRing<UART_EGRESS_RING_BYTES>,
) {
    let mut tx_chunk = [0u8; UART_EGRESS_CHUNK_BYTES];
    while !egress_ring.is_empty() {
        let chunk_len = egress_ring.pop_into(&mut tx_chunk);
        if chunk_len == 0 {
            break;
        }
        match uart_tx.write(&tx_chunk[..chunk_len]).await {
            Ok(0) | Err(_) => {
                egress_ring.clear();
                break;
            }
            Ok(written) if written < chunk_len => {
                egress_ring.push_overwrite_slice(&tx_chunk[written..chunk_len]);
                break;
            }
            Ok(_) => {
                let _ = uart_tx.flush().await;
            }
        }
    }
}

async fn read_uart_frame_lossy(uart_rx: &mut impl Read, frame: &mut [u8; FRAME_SIZE]) {
    loop {
        if uart_rx.read_exact(frame).await.is_ok() {
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
