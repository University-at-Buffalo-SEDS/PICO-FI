#![no_std]
#![no_main]

//! Firmware entry point and high-level bridge role selection.

mod bridge;
mod config;
mod net;
mod protocol;
mod shell;
mod storage;

use bridge::spi::{init_upstream_spi, report_spi_probe};
use config::{BridgeConfig, BridgeMode, UpstreamMode};
use embassy_executor::{Executor, Spawner};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::UART0;
use embassy_rp::uart::{self, BufferedUart};
use embassy_time::Timer;
#[allow(unused_imports)]
use panic_halt as _;
use portable_atomic::{AtomicBool, Ordering};
use shell::{configuration_shell, write_banner, writeln_line};
use static_cell::StaticCell;
use storage::ConfigStorage;

// Interrupt bindings required by the buffered UART driver.
bind_interrupts!(struct Irqs {
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
});

/// Static TX buffer used by the boot/control UART.
static UART_TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

/// Static RX buffer used by the boot/control UART.
static UART_RX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

/// Single-core Embassy executor used by the firmware.
static EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Shared link-state flag consumed by status reporting and heartbeat LED behavior.
static LINK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Delay before the client role attempts its first outbound TCP connection.
const CLIENT_STARTUP_DELAY_MS: u64 = 250;

/// Delay between failed or closed client reconnect attempts.
const CLIENT_RECONNECT_DELAY_MS: u64 = 500;

/// Timeout applied to outbound TCP connection establishment.
const LINK_CONNECT_TIMEOUT_MS: u64 = 1_500;

/// Timeout applied to the bridge handshake exchange after TCP connects.
const LINK_HANDSHAKE_TIMEOUT_MS: u64 = 2_000;

/// Fixed magic exchanged by both peers to confirm protocol compatibility.
const LINK_HANDSHAKE_MAGIC: &[u8] = b"PICOFI1";

/// Blinks the onboard LED while a bridge link is active.
#[embassy_executor::task]
async fn heartbeat_task(mut led: Output<'static>) {
    loop {
        if LINK_ACTIVE.load(Ordering::Relaxed) {
            led.toggle();
            Timer::after_millis(500).await;
        } else {
            led.set_low();
            Timer::after_millis(200).await;
        }
    }
}

/// Performs all peripheral setup and dispatches into the selected bridge role.
#[embassy_executor::task]
async fn app(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut status_led = Some(Output::new(p.PIN_25, Level::Low));
    for _ in 0..3 {
        status_led.as_mut().unwrap().toggle();
        Timer::after_millis(100).await;
        status_led.as_mut().unwrap().toggle();
        Timer::after_millis(100).await;
    }

    let mut uart_config = uart::Config::default();
    uart_config.baudrate = 115_200;
    let mut uart = BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        UART_TX_BUF.init([0; 512]),
        UART_RX_BUF.init([0; 512]),
        uart_config,
    );

    let mut config_storage = ConfigStorage::new(p.FLASH);
    let initial_config = config_storage.load().unwrap_or_default();
    let _ = write_banner(&mut uart).await;
    let bridge_config = configuration_shell(&mut uart, &mut config_storage, initial_config).await;
    if !matches!(bridge_config.upstream_mode, UpstreamMode::Test) {
        spawner.must_spawn(heartbeat_task(status_led.take().unwrap()));
    }

    let mut upstream_spi = if matches!(bridge_config.upstream_mode, UpstreamMode::Spi) {
        let mut spi = init_upstream_spi(p.SPI1, p.PIN_10, p.PIN_11, p.PIN_12, p.PIN_13);
        let _ = report_spi_probe(&mut uart, &mut spi).await;
        Some(spi)
    } else {
        None
    };

    let stack = match net::bring_up_network(
        spawner,
        p.SPI0,
        p.PIN_16,
        p.PIN_17,
        p.PIN_18,
        p.PIN_19,
        p.PIN_20,
        p.PIN_21,
        p.DMA_CH0,
        p.DMA_CH1,
        bridge_config,
    )
    .await
    {
        Ok(stack) => stack,
        Err(err) => loop {
            let _ = writeln_line(&mut uart, err).await;
            Timer::after_secs(1).await;
        },
    };

    let _ = writeln_line(&mut uart, "network ready").await;
    let result = run_bridge_mode(
        &mut uart,
        stack,
        bridge_config,
        upstream_spi.as_mut(),
        status_led.as_mut(),
    )
    .await;

    if result.is_err() {
        let _ = writeln_line(&mut uart, "bridge stopped").await;
    }

    loop {
        Timer::after_secs(1).await;
    }
}

/// Selects the correct bridge implementation for the configured role and upstream transport.
async fn run_bridge_mode(
    uart: &mut BufferedUart,
    stack: embassy_net::Stack<'static>,
    bridge_config: BridgeConfig,
    upstream_spi: Option<&mut bridge::spi::UpstreamSpiDevice>,
    status_led: Option<&mut Output<'static>>,
) -> Result<(), ()> {
    match (bridge_config.bridge_mode, bridge_config.upstream_mode) {
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Uart) => {
            bridge::uart::run_client(
                uart,
                stack,
                host,
                port,
                bridge_config,
                &LINK_ACTIVE,
                CLIENT_STARTUP_DELAY_MS,
                CLIENT_RECONNECT_DELAY_MS,
                LINK_CONNECT_TIMEOUT_MS,
                LINK_HANDSHAKE_TIMEOUT_MS,
                LINK_HANDSHAKE_MAGIC,
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Uart) => {
            bridge::uart::run_server(
                uart,
                stack,
                port,
                bridge_config,
                &LINK_ACTIVE,
                LINK_HANDSHAKE_TIMEOUT_MS,
                LINK_HANDSHAKE_MAGIC,
            )
            .await
        }
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Spi) => match upstream_spi {
            Some(spi) => {
                bridge::spi::run_client(
                    uart,
                    stack,
                    host,
                    port,
                    spi,
                    bridge_config,
                    &LINK_ACTIVE,
                    CLIENT_STARTUP_DELAY_MS,
                    CLIENT_RECONNECT_DELAY_MS,
                    LINK_CONNECT_TIMEOUT_MS,
                    LINK_HANDSHAKE_TIMEOUT_MS,
                    LINK_HANDSHAKE_MAGIC,
                )
                .await
            }
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::Spi) => match upstream_spi {
            Some(spi) => {
                bridge::spi::run_server(
                    uart,
                    stack,
                    port,
                    spi,
                    bridge_config,
                    &LINK_ACTIVE,
                    LINK_HANDSHAKE_TIMEOUT_MS,
                    LINK_HANDSHAKE_MAGIC,
                )
                .await
            }
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Test) => {
            bridge::test::run_client(
                uart,
                stack,
                host,
                port,
                status_led.unwrap(),
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Test) => {
            bridge::test::run_server(uart, stack, port, status_led.unwrap()).await
        }
    }
}

/// Starts the Embassy executor and launches the async application task.
#[cortex_m_rt::entry]
fn main() -> ! {
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.must_spawn(app(spawner));
    })
}
