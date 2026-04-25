//! I2C upstream bridge implementation.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::i2c_task::{I2C_PACKET_MAX, I2cPacket};
use crate::bridge::overwrite_queue::{
    I2C_PACKET_QUEUE_BYTES, I2C_PACKET_QUEUE_DEPTH, OverwriteQueue,
};
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{
    connect_with_timeout, exchange_link_handshake, read_bridge_frame, write_bridge_frame,
};
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use heapless::Vec;
use portable_atomic::{AtomicBool, Ordering};

const KIND_DATA: u8 = 0x01;
const KIND_COMMAND: u8 = 0x02;
const KIND_ERROR: u8 = 0x7F;

pub async fn run_client(
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    i2c_rx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
    i2c_tx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    Timer::after_millis(runtime.startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_nagle_enabled(false);
        socket.set_keep_alive(Some(Duration::from_secs(3)));

        runtime.link_active.store(false, Ordering::Relaxed);
        if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms)
            .await
            .is_err()
        {
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
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
    i2c_rx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
    i2c_tx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_nagle_enabled(false);
        socket.set_keep_alive(Some(Duration::from_secs(3)));

        if socket.accept(port).await.is_err() {
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
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
    i2c_rx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
    i2c_tx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
) -> Result<(), ()> {
    let mut net_buf = [0u8; I2C_PACKET_MAX];

    loop {
        match select(i2c_rx.pop(), read_bridge_frame(socket, &mut net_buf)).await {
            Either::First(packet) => {
                handle_i2c_request(packet, Some(socket), bridge_config, link_active, i2c_tx)
                    .await?;
            }
            Either::Second(Ok(net_n)) => {
                i2c_tx.push_overwrite(make_packet(KIND_DATA, &net_buf[..net_n])?);
            }
            Either::Second(Err(_)) => return Err(()),
        }

        yield_now().await;
    }
}

async fn handle_i2c_request(
    packet: I2cPacket,
    socket: Option<&mut TcpSocket<'_>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    i2c_tx: &'static OverwriteQueue<I2cPacket, I2C_PACKET_QUEUE_DEPTH, I2C_PACKET_QUEUE_BYTES>,
) -> Result<(), ()> {
    match packet.kind {
        KIND_DATA => {
            let payload = packet.payload.as_slice();
            if let Some(socket) = socket {
                if !payload.is_empty() {
                    write_bridge_frame(socket, payload).await?;
                }
                i2c_tx.push_overwrite(make_packet(KIND_DATA, b"")?);
            } else if !payload.is_empty() {
                i2c_tx.push_overwrite(make_packet(KIND_DATA, b"")?);
            }
            Ok(())
        }
        KIND_COMMAND => {
            let line = trim_ascii_line(packet.payload.as_slice());
            let response = render_local_bridge_command(bridge_config, link_active, line);
            i2c_tx.push_overwrite(make_packet(KIND_COMMAND, response.as_bytes())?);
            Ok(())
        }
        _ => {
            i2c_tx.push_overwrite(make_packet(KIND_ERROR, b"error invalid i2c frame")?);
            Ok(())
        }
    }
}

fn make_packet(kind: u8, payload: &[u8]) -> Result<I2cPacket, ()> {
    let mut data = Vec::<u8, I2C_PACKET_MAX>::new();
    data.extend_from_slice(payload).map_err(|_| ())?;
    Ok(I2cPacket {
        kind,
        payload: data,
    })
}
