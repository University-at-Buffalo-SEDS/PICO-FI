//! Core 1 owns UART and its IRQ; core 0 owns W5500, TCP and discovery.
use super::commands::{render_local_bridge_command, trim_ascii_line};
use super::runtime::BridgeRuntime;
use super::uart_mailbox::*;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake};
use crate::protocol::stream::FrameReader;
use embassy_futures::{join::join, yield_now};
use embassy_net::{Ipv4Address, Stack, tcp::TcpSocket};
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use portable_atomic::Ordering;

pub async fn uart_worker(mut uart: BufferedUart, config: BridgeConfig) -> ! {
    UART_CORE_ID.store(
        embassy_rp::multicore::current_core() as u32,
        Ordering::Release,
    );
    let (tx, rx) = uart.split_ref();
    join(uart_receive(rx, config), uart_transmit(tx)).await;
    unreachable!()
}
async fn uart_receive(rx: &mut impl Read, config: BridgeConfig) {
    let mut reader = FrameReader::new([[0xa5, 0x5a], [0xa6, 0x5b]], true);
    let mut packet = Packet::new(0xa5);
    loop {
        match reader.read(rx, packet.buffer()).await {
            Ok((kind, len)) => {
                packet.finish(kind, len);
                if kind == 0xa6 {
                    let reply = render_local_bridge_command(
                        config,
                        &crate::LINK_ACTIVE,
                        trim_ascii_line(packet.payload()),
                    );
                    let mut response = Packet::new(0xa6);
                    response.buffer()[..reply.len()].copy_from_slice(reply.as_bytes());
                    response.finish(0xa6, reply.len());
                    // A blocked local reply must not stop upstream reception.
                    if !TO_UART.try_push_ref(&response) {
                        RX_DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                } else if crate::LINK_ACTIVE.load(Ordering::Acquire) {
                    admit_uart(&packet);
                }
            }
            Err(()) => {
                RX_ERRORS.fetch_add(1, Ordering::Relaxed);
                Timer::after_millis(1).await;
            }
        }
        yield_now().await;
    }
}
async fn uart_transmit(tx: &mut impl Write) {
    loop {
        let packet = TO_UART.pop().await;
        let n = packet.payload().len();
        let header = [
            packet.kind,
            if packet.kind == 0xa6 { 0x5b } else { 0x5a },
            n as u8,
            (n >> 8) as u8,
        ];
        if tx.write_all(&header).await.is_err() || tx.write_all(packet.payload()).await.is_err() {
            RX_ERRORS.fetch_add(1, Ordering::Relaxed);
        } else {
            TX_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        yield_now().await;
    }
}
async fn network_session(socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    let (mut rx, mut tx) = socket.split();
    forward_tcp(&mut rx, &mut tx, &TO_NETWORK, &TO_UART).await
}

pub async fn run(
    stack: Stack<'static>,
    host: Option<[u8; 4]>,
    port: u16,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    NET_CORE_ID.store(
        embassy_rp::multicore::current_core() as u32,
        Ordering::Release,
    );
    if host.is_some() {
        Timer::after_millis(runtime.startup_delay_ms).await;
    }
    loop {
        let mut rx = [0; 8192];
        let mut tx = [0; 8192];
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_nagle_enabled(false);
        socket.set_keep_alive(Some(Duration::from_secs(3)));
        crate::LINK_ACTIVE.store(false, Ordering::Release);
        // Disconnected telemetry must not delay fresh data on reconnection.
        while TO_NETWORK.try_pop().is_some() {}
        let connected = match host {
            Some(h) => {
                connect_with_timeout(
                    &mut socket,
                    Ipv4Address::new(h[0], h[1], h[2], h[3]),
                    port,
                    runtime.connect_timeout_ms,
                )
                .await
            }
            None => socket.accept(port).await.map_err(|_| ()),
        };
        socket.set_timeout(Some(Duration::from_millis(runtime.session_timeout_ms)));
        if connected.is_ok()
            && exchange_link_handshake(
                &mut socket,
                host.is_some(),
                runtime.handshake_magic,
                runtime.handshake_timeout_ms,
            )
            .await
            .is_ok()
        {
            crate::LINK_ACTIVE.store(true, Ordering::Release);
            let _ = network_session(&mut socket).await;
        }
        crate::LINK_ACTIVE.store(false, Ordering::Release);
        socket.abort();
        let _ = socket.flush().await;
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}
