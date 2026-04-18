//! TCP test mode used for quick LED and connectivity checks.

use crate::bridge::commands::trim_ascii_line;
use crate::net::write_socket;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::gpio::Output;
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};

/// Runs the test bridge in TCP client mode.
pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    led: &mut Output<'static>,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(30)));

    socket.connect((remote, port)).await.map_err(|_| ())?;

    session(uart, &mut socket, led).await
}

/// Runs the test bridge in TCP server mode.
pub async fn run_server(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    led: &mut Output<'static>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(None);

        socket.accept(port).await.map_err(|_| ())?;

        let _ = session(uart, &mut socket, led).await;
    }
}

/// Serves one active test-mode TCP session.
async fn session(
    _uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    led: &mut Output<'static>,
) -> Result<(), ()> {
    write_socket(socket, b"pico-fi test mode\r\n").await?;
    write_socket(
        socket,
        b"commands: ping, led on, led off, led toggle, led blink <ms>, led status, help\r\n",
    )
    .await?;

    let mut net_buf = [0u8; 256];
    let mut led_on = false;

    loop {
        let net_n = socket.read(&mut net_buf).await.map_err(|_| ())?;
        if net_n == 0 {
            return Ok(());
        }

        let line = trim_ascii_line(&net_buf[..net_n]);
        let response = handle_command(line, led, &mut led_on).await;
        write_socket(socket, response.as_bytes()).await?;
        write_socket(socket, b"\r\n").await?;
    }
}

/// Executes a single test command and returns the response string.
async fn handle_command<'a>(
    line: &'a str,
    led: &mut Output<'static>,
    led_on: &mut bool,
) -> &'a str {
    match line {
        "ping" => "pong",
        "help" => "commands: ping, led on, led off, led toggle, led blink <ms>, led status",
        "led on" => {
            led.set_high();
            *led_on = true;
            "ok led on"
        }
        "led off" => {
            led.set_low();
            *led_on = false;
            "ok led off"
        }
        "led toggle" => {
            led.toggle();
            *led_on = !*led_on;
            if *led_on { "ok led on" } else { "ok led off" }
        }
        "led status" => {
            if *led_on {
                "led on"
            } else {
                "led off"
            }
        }
        _ => {
            if let Some(delay_ms) = parse_blink_command(line) {
                for _ in 0..4 {
                    led.toggle();
                    Timer::after_millis(delay_ms).await;
                    led.toggle();
                    Timer::after_millis(delay_ms).await;
                }
                *led_on = false;
                "ok blink complete"
            } else {
                "error unknown command"
            }
        }
    }
}

/// Parses the optional `led blink <ms>` command form.
fn parse_blink_command(line: &str) -> Option<u64> {
    let value = line.strip_prefix("led blink ")?;
    value.parse::<u64>().ok()
}
