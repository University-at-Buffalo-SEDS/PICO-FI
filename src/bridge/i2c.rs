//! I2C upstream bridge implementation.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::i2c_task::I2cFrame;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::i2c::{
    RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame, parse_request_frame,
};
use embassy_futures::select::{Either, select};
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicBool, Ordering};

pub async fn run_client(
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
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

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            i2c_rx,
            i2c_tx,
        )
        .await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}

pub async fn run_server(
    stack: Stack<'static>,
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
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

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            i2c_rx,
            i2c_tx,
        )
        .await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

async fn session(
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> Result<(), ()> {
    let mut net_buf = [0u8; 256];

    loop {
        match select(i2c_rx.receive(), socket.read(&mut net_buf)).await {
            Either::First(frame) => {
                handle_i2c_request(frame, Some(socket), bridge_config, link_active, i2c_tx).await?;
            }
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                let response = make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]);
                i2c_tx.send(I2cFrame { data: response }).await;
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

async fn handle_i2c_request(
    frame: I2cFrame,
    socket: Option<&mut TcpSocket<'_>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> Result<(), ()> {
    match parse_request_frame(&frame.data) {
        Some(RequestFrame::Data(payload)) => {
            if let Some(socket) = socket {
                if !payload.is_empty() {
                    write_socket(socket, payload).await?;
                }
            } else if !payload.is_empty() {
                let response = make_response_frame(RESP_DATA_MAGIC, b"");
                i2c_tx.send(I2cFrame { data: response }).await;
            }
            Ok(())
        }
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response =
                render_local_bridge_command(bridge_config, link_active, line);
            let frame = make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes());
            i2c_tx.send(I2cFrame { data: frame }).await;
            Ok(())
        }
        None => {
            let response = make_response_frame(RESP_COMMAND_MAGIC, b"error invalid i2c frame");
            i2c_tx.send(I2cFrame { data: response }).await;
            Ok(())
        }
    }
}
