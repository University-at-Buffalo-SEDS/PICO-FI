//! USB CDC upstream bridge implementation.

use crate::bridge::commands::render_local_bridge_command;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake};
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_acm::{Receiver, Sender};
use heapless::String;
use portable_atomic::{AtomicBool, Ordering};

struct LocalCommandState {
    at_line_start: bool,
    active: bool,
    buf: String<256>,
}

impl LocalCommandState {
    fn new() -> Self {
        Self {
            at_line_start: true,
            active: false,
            buf: String::new(),
        }
    }
}

async fn write_usb_packet(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    bytes: &[u8],
) -> Result<(), ()> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = bytes.len().min(offset + 64);
        sender.write_packet(&bytes[offset..end]).await.map_err(|_| ())?;
        offset = end;
    }
    Ok(())
}

async fn write_usb_line(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    value: &str,
) -> Result<(), ()> {
    write_usb_packet(sender, value.as_bytes()).await?;
    write_usb_packet(sender, b"\r\n").await
}

async fn handle_local_command(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    line: &str,
) -> Result<bool, ()> {
    if !line.starts_with('/') {
        return Ok(false);
    }

    let response = render_local_bridge_command(bridge_config, link_active, line);
    write_usb_line(sender, response.as_str()).await?;
    Ok(true)
}

async fn handle_usb_input(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    bytes: &[u8],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    command_state: &mut LocalCommandState,
    mut socket: Option<&mut TcpSocket<'_>>,
) -> Result<(), ()> {
    for &byte in bytes {
        if command_state.active {
            match byte {
                b'\r' => {}
                b'\n' => {
                    let _ = handle_local_command(
                        sender,
                        bridge_config,
                        link_active,
                        command_state.buf.as_str(),
                    )
                    .await?;
                    command_state.buf.clear();
                    command_state.active = false;
                    command_state.at_line_start = true;
                }
                byte if byte.is_ascii_graphic() || byte == b' ' => {
                    if command_state.buf.push(byte as char).is_err() {
                        flush_command_bytes(command_state, socket.as_deref_mut()).await?;
                        forward_usb_bytes(socket.as_deref_mut(), &[byte]).await?;
                        command_state.at_line_start = false;
                    }
                }
                _ => {
                    flush_command_bytes(command_state, socket.as_deref_mut()).await?;
                    forward_usb_bytes(socket.as_deref_mut(), &[byte]).await?;
                    command_state.at_line_start = false;
                }
            }
            continue;
        }

        if command_state.at_line_start && byte == b'/' {
            command_state.active = true;
            command_state.buf.clear();
            let _ = command_state.buf.push('/');
            continue;
        }

        forward_usb_bytes(socket.as_deref_mut(), &[byte]).await?;
        if byte == b'\n' {
            command_state.at_line_start = true;
        } else if byte != b'\r' {
            command_state.at_line_start = false;
        }
    }
    Ok(())
}

async fn flush_command_bytes(
    command_state: &mut LocalCommandState,
    socket: Option<&mut TcpSocket<'_>>,
) -> Result<(), ()> {
    if !command_state.buf.is_empty() {
        forward_usb_bytes(socket, command_state.buf.as_bytes()).await?;
        command_state.buf.clear();
    }
    command_state.active = false;
    Ok(())
}

async fn forward_usb_bytes(
    socket: Option<&mut TcpSocket<'_>>,
    bytes: &[u8],
) -> Result<(), ()> {
    if let Some(socket) = socket {
        if !bytes.is_empty() {
            crate::net::write_socket(socket, bytes).await?;
        }
    }
    Ok(())
}

async fn session(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    receiver: &mut Receiver<'static, Driver<'static, USB>>,
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
    let mut usb_buf = [0u8; 256];
    let mut net_buf = [0u8; 256];
    let mut command_state = LocalCommandState::new();

    loop {
        match select(receiver.read_packet(&mut usb_buf), socket.read(&mut net_buf)).await {
            Either::First(Ok(usb_n)) => {
                if usb_n == 0 {
                    yield_now().await;
                    continue;
                }
                handle_usb_input(
                    sender,
                    &usb_buf[..usb_n],
                    bridge_config,
                    link_active,
                    &mut command_state,
                    Some(socket),
                )
                .await?;
            }
            Either::First(Err(_)) => return Err(()),
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                write_usb_packet(sender, &net_buf[..net_n]).await?;
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

pub async fn run_client(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    receiver: &mut Receiver<'static, Driver<'static, USB>>,
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
        if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms)
            .await
            .is_err()
        {
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
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

        let _ = session(sender, receiver, &mut socket, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}

pub async fn run_server(
    sender: &mut Sender<'static, Driver<'static, USB>>,
    receiver: &mut Receiver<'static, Driver<'static, USB>>,
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

        if socket.accept(port).await.is_err() {
            return Err(());
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

        let _ = session(sender, receiver, &mut socket, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}
