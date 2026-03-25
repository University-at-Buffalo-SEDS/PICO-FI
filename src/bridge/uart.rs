//! UART upstream bridge implementation.

use crate::bridge::commands::render_local_bridge_command;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::shell::writeln_line;
use embassy_futures::select::{Either, select};
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use heapless::String;
use portable_atomic::{AtomicBool, Ordering};

/// Runs the UART bridge in TCP client mode with reconnect behavior.
pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    startup_delay_ms: u64,
    reconnect_delay_ms: u64,
    connect_timeout_ms: u64,
    handshake_timeout_ms: u64,
    handshake_magic: &[u8],
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let _ = writeln_line(uart, "stabilizing before first connect").await;
    Timer::after_millis(startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "connecting").await;
        if connect_with_timeout(&mut socket, remote, port, connect_timeout_ms)
            .await
            .is_err()
        {
            let _ = writeln_line(uart, "connect failed").await;
            Timer::after_millis(reconnect_delay_ms).await;
            continue;
        }
        let _ = writeln_line(uart, "tcp connected").await;
        if exchange_link_handshake(&mut socket, true, handshake_magic, handshake_timeout_ms)
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            Timer::after_millis(reconnect_delay_ms).await;
            continue;
        }
        link_active.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = session(uart, &mut socket, bridge_config, link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "server disconnected").await;
        let _ = writeln_line(uart, "cooling down before reconnect").await;
        Timer::after_millis(reconnect_delay_ms).await;
    }
}

/// Runs the UART bridge in TCP server mode.
pub async fn run_server(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    handshake_timeout_ms: u64,
    handshake_magic: &[u8],
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        let _ = writeln_line(uart, "waiting for tcp client").await;
        socket.accept(port).await.map_err(|_| ())?;
        let _ = writeln_line(uart, "tcp client connected").await;
        if exchange_link_handshake(&mut socket, false, handshake_magic, handshake_timeout_ms)
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            link_active.store(false, Ordering::Relaxed);
            continue;
        }
        link_active.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = session(uart, &mut socket, bridge_config, link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "client disconnected").await;
    }
}

/// Relays bytes between UART and the bridged TCP socket.
async fn session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
    let mut uart_buf = [0u8; 256];
    let mut net_buf = [0u8; 256];
    let mut line_buf = String::<256>::new();

    loop {
        match select(uart.read(&mut uart_buf), socket.read(&mut net_buf)).await {
            Either::First(Ok(uart_n)) => {
                if uart_n == 0 {
                    Timer::after_millis(5).await;
                    continue;
                }
                for &byte in &uart_buf[..uart_n] {
                    match byte {
                        b'\r' => {}
                        b'\n' => {
                            if handle_local_command(uart, bridge_config, link_active, line_buf.as_str())
                                .await?
                            {
                                line_buf.clear();
                                continue;
                            }
                            write_socket(socket, line_buf.as_bytes()).await?;
                            write_socket(socket, b"\n").await?;
                            line_buf.clear();
                        }
                        byte if byte.is_ascii() => {
                            let _ = line_buf.push(byte as char);
                        }
                        _ => {}
                    }
                }
            }
            Either::First(Err(_)) => return Err(()),
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                uart.write_all(&net_buf[..net_n]).await.map_err(|_| ())?;
                uart.flush().await.map_err(|_| ())?;
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

/// Handles a slash-prefixed local bridge command on the UART control path.
async fn handle_local_command(
    uart: &mut BufferedUart,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    line: &str,
) -> Result<bool, ()> {
    if !line.starts_with('/') {
        return Ok(false);
    }

    let response = render_local_bridge_command(bridge_config, link_active, line);
    writeln_line(uart, response.as_str()).await?;
    Ok(true)
}
