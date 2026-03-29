//! UART upstream bridge implementation.

use crate::bridge::commands::signal_led_activity;
use crate::bridge::overwrite_queue::OverwriteByteRing;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_futures::yield_now;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use portable_atomic::{AtomicBool, Ordering};
const UART_EGRESS_RING_BYTES: usize = 2048;
const UART_EGRESS_CHUNK_BYTES: usize = 256;

/// Runs the UART bridge in TCP client mode with reconnect behavior.
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
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        runtime.link_active.store(false, Ordering::Relaxed);
        if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms)
            .await
            .is_err()
        {
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
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

/// Runs the UART bridge in TCP server mode.
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
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        if socket.accept(port).await.is_err() {
            return Err(());
        }
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

/// Relays bytes between UART and the bridged TCP socket.
async fn session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    _bridge_config: BridgeConfig,
    _link_active: &AtomicBool,
) -> Result<(), ()> {
    let mut uart_buf = [0u8; 1024];
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
            match select3(
                uart_rx.read(&mut uart_buf),
                socket.read(&mut net_buf),
                uart_tx.write(&tx_chunk[tx_chunk_pos..tx_chunk_len]),
            )
            .await
            {
                Either3::First(Ok(uart_n)) => {
                    if uart_n == 0 {
                        yield_now().await;
                        continue;
                    }
                    forward_uart_bytes(Some(socket), &uart_buf[..uart_n]).await?;
                }
                Either3::First(Err(_)) => return Err(()),
                Either3::Second(Ok(net_n)) => {
                    if net_n == 0 {
                        return Ok(());
                    }
                    egress_ring.push_overwrite_slice(&net_buf[..net_n]);
                }
                Either3::Second(Err(_)) => return Err(()),
                Either3::Third(Ok(written)) => {
                    if written == 0 {
                        return Err(());
                    }
                    tx_chunk_pos = (tx_chunk_pos + written).min(tx_chunk_len);
                    if tx_chunk_pos >= tx_chunk_len {
                        tx_chunk_pos = 0;
                        tx_chunk_len = 0;
                    }
                }
                Either3::Third(Err(_)) => return Err(()),
            }
        } else {
            match select(uart_rx.read(&mut uart_buf), socket.read(&mut net_buf)).await {
                Either::First(Ok(uart_n)) => {
                    if uart_n == 0 {
                        yield_now().await;
                        continue;
                    }
                    forward_uart_bytes(Some(socket), &uart_buf[..uart_n]).await?;
                }
                Either::First(Err(_)) => return Err(()),
                Either::Second(Ok(net_n)) => {
                    if net_n == 0 {
                        return Ok(());
                    }
                    egress_ring.push_overwrite_slice(&net_buf[..net_n]);
                }
                Either::Second(Err(_)) => return Err(()),
            }
        }
    }
}

async fn forward_uart_bytes(
    socket: Option<&mut TcpSocket<'_>>,
    bytes: &[u8],
) -> Result<(), ()> {
    if let Some(socket) = socket {
        if !bytes.is_empty() {
            signal_led_activity();
            write_socket(socket, bytes).await?;
        }
    }
    Ok(())
}
