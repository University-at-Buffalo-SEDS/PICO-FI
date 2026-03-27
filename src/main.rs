#![no_std]
#![no_main]

//! Firmware entry point and high-level bridge role selection.

mod bridge;
mod config;
mod net;
mod protocol;
mod shell;
mod storage;

use bridge::i2c_task::{i2c_poll_task, I2cFrame};
use bridge::spi_task::{spi_poll_task, SpiFrame};
use bridge::runtime::BridgeRuntime;
use bridge::commands::{set_led_state, take_led_command};
use config::{BridgeConfig, BridgeMode, UpstreamMode};
use embassy_executor::{Executor, Spawner};
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::i2c_slave::{Config as I2cSlaveConfig, I2cSlave};
use embassy_rp::interrupt::InterruptExt as _;
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, I2C0, UART0};
use embassy_rp::uart::{self, BufferedUart};
use embassy_sync::channel::Channel;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
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
    I2C0_IRQ => embassy_rp::i2c::InterruptHandler<I2C0>;
});

/// Static TX buffer used by the boot/control UART.
static UART_TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

/// Static RX buffer used by the boot/control UART.
static UART_RX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

/// Single-core Embassy executor used by the firmware.
static EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Channel for I2C frames from polling task to bridge session.
static I2C_FRAME_CHANNEL: Channel<CriticalSectionRawMutex, I2cFrame, 4> = Channel::new();

/// Channel for response frames from bridge session back to the I2C polling task.
static I2C_RESPONSE_CHANNEL: Channel<CriticalSectionRawMutex, I2cFrame, 4> = Channel::new();
static SPI_FRAME_CHANNEL: Channel<CriticalSectionRawMutex, SpiFrame, 4> = Channel::new();
static SPI_RESPONSE_CHANNEL: Channel<CriticalSectionRawMutex, SpiFrame, 4> = Channel::new();

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

fn disable_uart0() {
    let regs = rp_pac::UART0;
    regs.uartimsc().write_value(Default::default());
    regs.uartdmacr().write_value(Default::default());
    regs.uartcr().write(|w| {
        w.set_uarten(false);
        w.set_txe(false);
        w.set_rxe(false);
    });
    regs.uarticr().write(|w| {
        w.set_rtic(true);
        w.set_txic(true);
        w.set_rxic(true);
        w.set_feic(true);
        w.set_peic(true);
        w.set_beic(true);
        w.set_oeic(true);
    });
    embassy_rp::interrupt::Interrupt::UART0_IRQ.disable();
    embassy_rp::interrupt::Interrupt::UART0_IRQ.unpend();
}

/// Drives the onboard LED from either heartbeat mode or explicit local commands.
#[embassy_executor::task]
async fn heartbeat_task(mut led: Output<'static>) {
    let mut auto_mode = true;
    let mut led_on = false;
    loop {
        if let Some(command) = take_led_command() {
            match command {
                1 => {
                    auto_mode = false;
                    led.set_low();
                    led_on = false;
                }
                2 => {
                    auto_mode = false;
                    led.set_high();
                    led_on = true;
                }
                3 => {
                    auto_mode = false;
                    led.toggle();
                    led_on = !led_on;
                }
                _ => {
                    auto_mode = true;
                }
            }
            set_led_state(led_on);
        }

        if auto_mode {
            if LINK_ACTIVE.load(Ordering::Relaxed) {
                led.toggle();
                led_on = !led_on;
                set_led_state(led_on);
                Timer::after_millis(500).await;
            } else {
                led.set_low();
                led_on = false;
                set_led_state(false);
                Timer::after_millis(200).await;
            }
        } else {
            Timer::after_millis(50).await;
        }
    }
}

/// Performs all peripheral setup and dispatches into the selected bridge role on core 0.
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
    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        UART_TX_BUF.init([0; 512]),
        UART_RX_BUF.init([0; 512]),
        uart_config,
    );
    let mut uart = Some(uart);

    let mut config_storage = ConfigStorage::new(p.FLASH);
    let compiled_config = BridgeConfig::default();
    let initial_config = if matches!(compiled_config.upstream_mode, UpstreamMode::I2c | UpstreamMode::Spi) {
        compiled_config
    } else {
        config_storage.load().unwrap_or(compiled_config)
    };
    let bridge_config = configuration_shell(
        uart.as_mut()
            .expect("configuration shell requires the boot UART"),
        &mut config_storage,
        initial_config,
    )
    .await;
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

    let upstream_i2c = if matches!(bridge_config.upstream_mode, UpstreamMode::I2c) {
        // Release UART0 before reusing GPIO0/GPIO1 as the I2C0 upstream bus.
        drop(uart.take());
        disable_uart0();
        let mut i2c_config = I2cSlaveConfig::default();
        i2c_config.addr = 0x55;
        i2c_config.general_call = false;
        i2c_config.sda_pullup = false;
        i2c_config.scl_pullup = false;
        let i2c = I2cSlave::new(
            p.I2C0,
            unsafe { embassy_rp::peripherals::PIN_1::steal() },
            unsafe { embassy_rp::peripherals::PIN_0::steal() },
            Irqs,
            i2c_config,
        );
        // Spawn dedicated I2C polling task.
        spawner.spawn(
            i2c_controller_task(
                i2c,
                bridge_config,
                &LINK_ACTIVE,
                I2C_FRAME_CHANNEL.sender(),
                I2C_RESPONSE_CHANNEL.receiver(),
            )
                .expect("i2c controller task token allocation failed"),
        );
        Some(())
    } else {
        None
    };
    let upstream_spi = if matches!(bridge_config.upstream_mode, UpstreamMode::Spi) {
        spawner.spawn(
            spi_controller_task(
                bridge_config,
                &LINK_ACTIVE,
                SPI_FRAME_CHANNEL.sender(),
                SPI_RESPONSE_CHANNEL.receiver(),
            )
            .expect("spi controller task token allocation failed"),
        );
        Some(())
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
        uart.as_mut(),
        stack,
        bridge_config,
        upstream_i2c.as_ref(),
        upstream_spi.as_ref(),
        status_led.as_mut(),
        I2C_FRAME_CHANNEL.receiver(),
        I2C_RESPONSE_CHANNEL.sender(),
        SPI_FRAME_CHANNEL.receiver(),
        SPI_RESPONSE_CHANNEL.sender(),
    )
    .await;

    let _ = result;

    loop {
        Timer::after_secs(1).await;
    }
}

/// Selects the correct bridge implementation for the configured role and upstream transport.
async fn run_bridge_mode(
    uart: Option<&mut BufferedUart>,
    stack: embassy_net::Stack<'static>,
    bridge_config: BridgeConfig,
    upstream_i2c_enabled: Option<&()>,
    upstream_spi_enabled: Option<&()>,
    status_led: Option<&mut Output<'static>>,
    i2c_rx: embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: embassy_sync::channel::Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    spi_rx: embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    spi_tx: embassy_sync::channel::Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
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
            bridge::uart::run_client(
                uart.expect("uart mode requires an active UART"),
                stack,
                host,
                port,
                bridge_config,
                runtime,
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Uart) => {
            bridge::uart::run_server(
                uart.expect("uart mode requires an active UART"),
                stack,
                port,
                bridge_config,
                runtime,
            )
            .await
        }
        (BridgeMode::TcpClient { host, port }, UpstreamMode::I2c) => match upstream_i2c_enabled {
            Some(_) => bridge::i2c::run_client(stack, host, port, bridge_config, runtime, i2c_rx, i2c_tx).await,
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::I2c) => match upstream_i2c_enabled {
            Some(_) => bridge::i2c::run_server(stack, port, bridge_config, runtime, i2c_rx, i2c_tx).await,
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Spi) => match upstream_spi_enabled {
            Some(_) => bridge::spi::run_client(
                uart.expect("spi mode keeps boot UART for diagnostics"),
                stack,
                host,
                port,
                bridge_config,
                runtime,
                spi_rx,
                spi_tx,
            )
            .await,
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::Spi) => match upstream_spi_enabled {
            Some(_) => bridge::spi::run_server(
                uart.expect("spi mode keeps boot UART for diagnostics"),
                stack,
                port,
                bridge_config,
                runtime,
                spi_rx,
                spi_tx,
            )
            .await,
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Test) => {
            bridge::test::run_client(
                uart.expect("test mode requires an active UART"),
                stack,
                host,
                port,
                status_led.expect("test client mode requires a status LED"),
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Test) => {
            bridge::test::run_server(
                uart.expect("test mode requires an active UART"),
                stack,
                port,
                status_led.expect("test server mode requires a status LED"),
            )
            .await
        }
    }
}

/// Dedicated task for continuous I2C polling.
#[embassy_executor::task]
async fn i2c_controller_task(
    mut i2c: I2cSlave<'static, I2C0>,
    bridge_config: BridgeConfig,
    link_active: &'static AtomicBool,
    tx: embassy_sync::channel::Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    rx_resp: embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) {
    i2c_poll_task(&mut i2c, bridge_config, link_active, tx, rx_resp).await
}

#[embassy_executor::task]
async fn spi_controller_task(
    bridge_config: BridgeConfig,
    link_active: &'static AtomicBool,
    tx: embassy_sync::channel::Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    rx_resp: embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
) {
    spi_poll_task(bridge_config, link_active, tx, rx_resp).await
}

/// Starts the Embassy executor and launches the async application task.
#[cortex_m_rt::entry]
fn main() -> ! {
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(app(spawner).expect("app task token allocation failed"));
    })
}

