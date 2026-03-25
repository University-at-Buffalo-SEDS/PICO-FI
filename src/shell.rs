//! UART-facing configuration shell and basic line output helpers.

use crate::config::{BridgeConfig, Command, apply_command, parse_command, render_config};
use crate::storage::ConfigStorage;
use embassy_futures::select::{Either, select};
use embassy_rp::uart::BufferedUart;
use embassy_time::Timer;
use embedded_io_async::{Read, Write};
use heapless::String;

/// Writes the boot banner and pre-start command summary.
pub async fn write_banner(uart: &mut BufferedUart) -> Result<(), ()> {
    writeln_line(uart, "").await?;
    writeln_line(uart, "pico-fi uart bridge").await?;
    writeln_line(uart, "booting with compiled config").await?;
    writeln_line(uart, "commands available before network starts:").await?;
    writeln_line(uart, "  show").await?;
    writeln_line(uart, "  set dhcp").await?;
    writeln_line(uart, "  set static <ip>/<prefix> <gateway> <dns>").await?;
    writeln_line(uart, "  set client <dest-ip> <port>").await?;
    writeln_line(uart, "  set server <listen-port>").await?;
    writeln_line(uart, "  set upstream <uart|spi|test>").await?;
    writeln_line(uart, "  reset").await?;
    writeln_line(uart, "  start").await
}

/// Runs the short boot-time UART shell that allows overriding the compiled config.
pub async fn configuration_shell(
    uart: &mut BufferedUart,
    storage: &mut ConfigStorage,
    initial_config: BridgeConfig,
) -> BridgeConfig {
    let mut config = initial_config;
    let rendered = render_config(&config);
    let _ = writeln_line(uart, "default:").await;
    let _ = writeln_line(uart, rendered.as_str()).await;
    let _ = writeln_line(
        uart,
        "auto-starting in 3 seconds; type commands to override or `start` now",
    )
    .await;

    for _ in 0..30 {
        let _ = write_str(uart, "> ").await;
        let mut line = String::<128>::new();
        match read_line_with_timeout(uart, &mut line, 100).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(()) => {
                let _ = writeln_line(uart, "uart read error").await;
                continue;
            }
        }

        match parse_command(line.as_str()) {
            Ok(Command::Help) => {
                let _ = write_banner(uart).await;
            }
            Ok(Command::Show) => {
                let rendered = render_config(&config);
                let _ = writeln_line(uart, rendered.as_str()).await;
            }
            Ok(Command::Reset) => {
                config = BridgeConfig::default();
                if storage.reset().is_err() {
                    let _ = writeln_line(uart, "failed to clear persisted config").await;
                }
                let rendered = render_config(&config);
                let _ = writeln_line(uart, rendered.as_str()).await;
            }
            Ok(command) => {
                if apply_command(&mut config, command) {
                    let rendered = render_config(&config);
                    let _ = writeln_line(uart, rendered.as_str()).await;
                    return config;
                }
                if storage.save(config).is_err() {
                    let _ = writeln_line(uart, "failed to persist config").await;
                }
                let rendered = render_config(&config);
                let _ = writeln_line(uart, rendered.as_str()).await;
            }
            Err(err) => {
                let _ = writeln_line(uart, err).await;
            }
        }
    }

    let _ = writeln_line(uart, "starting compiled config").await;
    config
}

/// Reads one editable line from UART while allowing the caller to poll on timeout.
pub async fn read_line_with_timeout(
    uart: &mut BufferedUart,
    line: &mut String<128>,
    timeout_ms: u64,
) -> Result<bool, ()> {
    let mut byte = [0u8; 1];

    loop {
        match select(uart.read_exact(&mut byte), Timer::after_millis(timeout_ms)).await {
            Either::First(Ok(())) => match byte[0] {
                b'\r' | b'\n' => {
                    let _ = writeln_line(uart, "").await;
                    return Ok(true);
                }
                0x08 | 0x7f => {
                    line.pop();
                }
                ch if ch.is_ascii_graphic() || ch == b' ' => {
                    if line.push(ch as char).is_ok() {
                        uart.write_all(&byte).await.map_err(|_| ())?;
                        uart.flush().await.map_err(|_| ())?;
                    }
                }
                _ => {}
            },
            Either::First(Err(_)) => return Err(()),
            Either::Second(_) => {
                if line.is_empty() {
                    return Ok(false);
                }
            }
        }
    }
}

/// Writes a string to UART without appending a newline.
pub async fn write_str(uart: &mut BufferedUart, value: &str) -> Result<(), ()> {
    uart.write_all(value.as_bytes()).await.map_err(|_| ())?;
    uart.flush().await.map_err(|_| ())
}

/// Writes a string to UART followed by CRLF.
pub async fn writeln_line(uart: &mut BufferedUart, value: &str) -> Result<(), ()> {
    uart.write_all(value.as_bytes()).await.map_err(|_| ())?;
    uart.write_all(b"\r\n").await.map_err(|_| ())?;
    uart.flush().await.map_err(|_| ())
}
