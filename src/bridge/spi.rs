//! SPI upstream bridge implementation.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::bridge::runtime::BridgeRuntime;
use crate::bridge::spi_task::SpiFrame;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::shell::writeln_line;
use crate::protocol::i2c::{
    RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame, parse_request_frame,
};
use embassy_futures::select::{Either, select};
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicBool, Ordering};

pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    spi_rx: &'static OverwriteQueue<SpiFrame, 8>,
    spi_tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    Timer::after_millis(runtime.startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        runtime.link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "spi client: connecting").await;
        if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms)
            .await
            .is_err()
        {
            let _ = writeln_line(uart, "spi client: connect failed").await;
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
        let _ = writeln_line(uart, "spi client: connected").await;
        if exchange_link_handshake(
            &mut socket,
            true,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
        .await
        .is_err()
        {
            let _ = writeln_line(uart, "spi client: handshake failed").await;
            socket.abort();
            let _ = socket.flush().await;
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
        let _ = writeln_line(uart, "spi client: handshake ok").await;
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            spi_rx,
            spi_tx,
        )
        .await;
        let _ = writeln_line(uart, "spi client: session closed").await;
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
    spi_rx: &'static OverwriteQueue<SpiFrame, 8>,
    spi_tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        let _ = writeln_line(uart, "spi server: listening").await;
        if socket.accept(port).await.is_err() {
            return Err(());
        }
        let _ = writeln_line(uart, "spi server: accepted").await;
        if exchange_link_handshake(
            &mut socket,
            false,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
        .await
        .is_err()
        {
            let _ = writeln_line(uart, "spi server: handshake failed").await;
            socket.abort();
            let _ = socket.flush().await;
            runtime.link_active.store(false, Ordering::Relaxed);
            continue;
        }
        let _ = writeln_line(uart, "spi server: handshake ok").await;
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            spi_rx,
            spi_tx,
        )
        .await;
        let _ = writeln_line(uart, "spi server: session closed").await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

async fn session(
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    spi_rx: &'static OverwriteQueue<SpiFrame, 8>,
    spi_tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Result<(), ()> {
    let mut net_buf = [0u8; 256];

    loop {
        match select(spi_rx.pop(), socket.read(&mut net_buf)).await {
            Either::First(frame) => {
                handle_spi_request(frame, Some(socket), bridge_config, link_active, spi_tx).await?;
            }
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                let response = make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]);
                spi_tx.push_overwrite(SpiFrame { data: response });
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

async fn handle_spi_request(
    frame: SpiFrame,
    socket: Option<&mut TcpSocket<'_>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    spi_tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Result<(), ()> {
    match parse_request_frame(&frame.data) {
        Some(RequestFrame::Data(payload)) => {
            if looks_like_local_command(payload) {
                let line = trim_ascii_line(payload);
                let response = render_local_bridge_command(bridge_config, link_active, line);
                let frame = make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes());
                spi_tx.push_overwrite(SpiFrame { data: frame });
                return Ok(());
            }
            if let Some(socket) = socket {
                if !payload.is_empty() {
                    write_socket(socket, payload).await?;
                }
                let response = make_response_frame(RESP_DATA_MAGIC, b"");
                spi_tx.push_overwrite(SpiFrame { data: response });
            } else {
                let response = make_response_frame(RESP_DATA_MAGIC, b"");
                spi_tx.push_overwrite(SpiFrame { data: response });
            }
            Ok(())
        }
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response = render_local_bridge_command(bridge_config, link_active, line);
            let frame = make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes());
            spi_tx.push_overwrite(SpiFrame { data: frame });
            Ok(())
        }
        None => {
            let response = make_response_frame(RESP_COMMAND_MAGIC, b"error invalid spi frame");
            spi_tx.push_overwrite(SpiFrame { data: response });
            Ok(())
        }
    }
}

fn looks_like_local_command(payload: &[u8]) -> bool {
    payload.first() == Some(&b'/')
        && payload
            .iter()
            .all(|&byte| byte == b'\n' || byte == b'\r' || (32..=126).contains(&byte))
}
