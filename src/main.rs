#![no_std]
#![no_main]

//! Firmware entry point and high-level bridge role selection.

mod bridge;
mod config;
mod net;
mod protocol;
mod shell;
mod storage;

use bridge::spi::init_upstream_spi;
use bridge::runtime::BridgeRuntime;
use config::{BridgeConfig, BridgeMode, UpstreamMode};
use embassy_executor::{Executor, Spawner};
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, UART0};
use embassy_rp::uart::{self, BufferedUart};
use embassy_time::Timer;
#[allow(unused_imports)]
use panic_halt as _;
use portable_atomic::{AtomicBool, Ordering};
use shell::configuration_shell;
use static_cell::StaticCell;
use storage::ConfigStorage;

// Interrupt bindings required by the buffered UART driver.
bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
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
        status_led
            .as_mut()
            .expect("status LED must exist during startup blink")
            .toggle();
        Timer::after_millis(100).await;
        status_led
            .as_mut()
            .expect("status LED must exist during startup blink")
            .toggle();
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
    let bridge_config = configuration_shell(&mut uart, &mut config_storage, initial_config).await;
    if !matches!(bridge_config.upstream_mode, UpstreamMode::Test) {
        spawner.spawn(
            heartbeat_task(
                status_led
                    .take()
                    .expect("heartbeat mode requires ownership of the status LED"),
            )
            .expect("heartbeat task token allocation failed"),
        );
    }

    let mut upstream_spi = if matches!(bridge_config.upstream_mode, UpstreamMode::Spi) {
        let spi = init_upstream_spi(
            p.SPI1,
            p.PIN_10,
            p.PIN_11,
            p.PIN_12,
            p.PIN_13,
            p.DMA_CH2,
            p.DMA_CH3,
        );
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
            let _ = err;
            Timer::after_secs(1).await;
        },
    };

    let result = run_bridge_mode(
        &mut uart,
        stack,
        bridge_config,
        upstream_spi.as_mut(),
        status_led.as_mut(),
    )
    .await;

    let _ = result;

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
    let runtime = BridgeRuntime {
        link_active: &LINK_ACTIVE,
        startup_delay_ms: CLIENT_STARTUP_DELAY_MS,
        reconnect_delay_ms: CLIENT_RECONNECT_DELAY_MS,
        connect_timeout_ms: LINK_CONNECT_TIMEOUT_MS,
        handshake_timeout_ms: LINK_HANDSHAKE_TIMEOUT_MS,
        handshake_magic: LINK_HANDSHAKE_MAGIC,
    };

    match (bridge_config.bridge_mode, bridge_config.upstream_mode) {
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Uart) => {
            bridge::uart::run_client(uart, stack, host, port, bridge_config, runtime).await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Uart) => {
            bridge::uart::run_server(uart, stack, port, bridge_config, runtime).await
        }
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Spi) => match upstream_spi {
            Some(spi) => {
                bridge::spi::run_client(uart, stack, host, port, spi, bridge_config, runtime).await
            }
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::Spi) => match upstream_spi {
            Some(spi) => {
                bridge::spi::run_server(uart, stack, port, spi, bridge_config, runtime).await
            }
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Test) => {
            bridge::test::run_client(
                uart,
                stack,
                host,
                port,
                status_led.expect("test client mode requires a status LED"),
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Test) => {
            bridge::test::run_server(
                uart,
                stack,
                port,
                status_led.expect("test server mode requires a status LED"),
            )
            .await
        }
    }
}

/// Starts the Embassy executor and launches the async application task.
#[cortex_m_rt::entry]
fn main() -> ! {
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(app(spawner).expect("app task token allocation failed"));
    })
}
