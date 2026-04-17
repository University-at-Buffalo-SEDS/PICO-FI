#![no_std]
#![no_main]

//! Firmware entry point and high-level bridge role selection.

mod bridge;
mod config;
mod net;
mod protocol;
mod shell;
mod storage;

use bridge::commands::{set_led_state, take_led_activity, take_led_command};
use bridge::i2c_task::{i2c_poll_task, I2cPacket};
use bridge::overwrite_queue::OverwriteQueue;
use bridge::runtime::BridgeRuntime;
use bridge::spi_frame::SpiFrame;
use bridge::spi_hw_task::spi_poll_task;
use config::{BridgeConfig, BridgeMode, UartPort, UpstreamMode, COMPILED_USB_DEVICE_NAMES};
use embassy_executor::{Executor, Spawner};
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::i2c_slave::{Config as I2cSlaveConfig, I2cSlave};
use embassy_rp::interrupt::InterruptExt as _;
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, I2C0, PIN_10, PIN_11, PIN_12, PIN_13, PIO1, SPI1, UART0,
    UART1, USB,
};
use embassy_rp::uart::{self, BufferedUart};
use embassy_rp::usb;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{
    CdcAcmClass, Receiver as UsbReceiver, Sender as UsbSender, State as UsbCdcState,
};
use embassy_usb::Builder as UsbBuilder;
use embassy_usb::{Config as UsbConfig, UsbDevice};
#[allow(unused_imports)]
use panic_halt as _;
use portable_atomic::{AtomicBool, Ordering};
use shell::{configuration_shell, drain_uart_rx};
use static_cell::StaticCell;
use storage::ConfigStorage;

// Interrupt bindings required by the buffered UART driver.
bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>, dma::InterruptHandler<DMA_CH2>, dma::InterruptHandler<DMA_CH3>;
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
    UART1_IRQ => uart::BufferedInterruptHandler<UART1>;
    I2C0_IRQ => embassy_rp::i2c::InterruptHandler<I2C0>;
    PIO1_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO1>;
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

/// Static TX buffer used by the boot/control UART.
static UART_TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();

/// Static RX buffer used by the boot/control UART.
static UART_RX_BUF: StaticCell<[u8; 512]> = StaticCell::new();
static USB_CONFIG_DESC_BUF: StaticCell<[u8; 256]> = StaticCell::new();
static USB_BOS_DESC_BUF: StaticCell<[u8; 256]> = StaticCell::new();
static USB_MSOS_DESC_BUF: StaticCell<[u8; 256]> = StaticCell::new();
static USB_CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
static USB_CDC_STATE: StaticCell<UsbCdcState<'static>> = StaticCell::new();

/// Single-core Embassy executor used by the firmware.
static EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Channel for I2C frames from polling task to bridge session.
static I2C_FRAME_QUEUE: OverwriteQueue<I2cPacket, 8> = OverwriteQueue::new();
static I2C_RESPONSE_QUEUE: OverwriteQueue<I2cPacket, 8> = OverwriteQueue::new();
static SPI_FRAME_QUEUE: OverwriteQueue<SpiFrame, 8> = OverwriteQueue::new();
static SPI_RESPONSE_QUEUE: OverwriteQueue<SpiFrame, 8> = OverwriteQueue::new();

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

/// Maximum idle time on an established TCP session before forcing reconnect/listen.
const LINK_SESSION_TIMEOUT_MS: u64 = 20_000;

/// Fixed magic exchanged by both peers to confirm protocol compatibility.
const LINK_HANDSHAKE_MAGIC: &[u8] = b"PICOFI1";

type UsbDriver = usb::Driver<'static, USB>;
type UsbCdcSender = UsbSender<'static, UsbDriver>;
type UsbCdcReceiver = UsbReceiver<'static, UsbDriver>;
type PicoUsbDevice = UsbDevice<'static, UsbDriver>;

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

fn disable_uart1() {
    let regs = rp_pac::UART1;
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
    embassy_rp::interrupt::Interrupt::UART1_IRQ.disable();
    embassy_rp::interrupt::Interrupt::UART1_IRQ.unpend();
}

fn disable_selected_uart(port: UartPort) {
    match port {
        UartPort::Uart0 => disable_uart0(),
        UartPort::Uart1 => disable_uart1(),
    }
}

/// Drives the onboard LED from either heartbeat mode or explicit local commands.
#[embassy_executor::task]
async fn heartbeat_task(mut led: Output<'static>) {
    let mut auto_mode = true;
    let mut led_on = false;
    loop {
        if take_led_activity() {
            let restore_on = led_on;
            led.set_high();
            set_led_state(true);
            Timer::after_millis(60).await;
            if restore_on {
                led.set_high();
                set_led_state(true);
            } else {
                led.set_low();
                set_led_state(false);
            }
            continue;
        }

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

#[embassy_executor::task]
async fn usb_device_task(mut device: PicoUsbDevice) {
    device.run().await;
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

    let mut config_storage = ConfigStorage::new(p.FLASH);
    let compiled_config = BridgeConfig::default();
    let initial_config = if matches!(
        compiled_config.upstream_mode,
        UpstreamMode::I2c
            | UpstreamMode::Spi
            | UpstreamMode::SpiEcho
            | UpstreamMode::SpiStatic
            | UpstreamMode::SpiLineHigh
    ) {
        compiled_config
    } else {
        config_storage.load().unwrap_or(compiled_config)
    };
    let mut uart_config = uart::Config::default();
    uart_config.baudrate = 115_200;
    let uart = match initial_config.uart_port {
        UartPort::Uart0 => BufferedUart::new(
            p.UART0,
            p.PIN_0,
            p.PIN_1,
            Irqs,
            UART_TX_BUF.init([0; 512]),
            UART_RX_BUF.init([0; 512]),
            uart_config,
        ),
        UartPort::Uart1 => BufferedUart::new(
            p.UART1,
            p.PIN_4,
            p.PIN_5,
            Irqs,
            UART_TX_BUF.init([0; 512]),
            UART_RX_BUF.init([0; 512]),
            uart_config,
        ),
    };
    let mut uart = Some(uart);
    let bridge_config = configuration_shell(
        uart.as_mut()
            .expect("configuration shell requires the boot UART"),
        &mut config_storage,
        initial_config,
    )
        .await;
    if let Some(uart) = uart.as_mut() {
        let _ = drain_uart_rx(uart, 10, 100).await;
    }
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
        // Release the selected UART, and fully disable UART0 if I2C needs GPIO0/GPIO1.
        drop(uart.take());
        disable_selected_uart(bridge_config.uart_port);
        let mut i2c_config = I2cSlaveConfig::default();
        i2c_config.addr = 0x55;
        i2c_config.general_call = false;
        i2c_config.sda_pullup = true;
        i2c_config.scl_pullup = true;
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
                &I2C_FRAME_QUEUE,
                &I2C_RESPONSE_QUEUE,
            )
                .expect("i2c controller task token allocation failed"),
        );
        Some(())
    } else {
        None
    };
    let upstream_spi = if matches!(
        bridge_config.upstream_mode,
        UpstreamMode::Spi
            | UpstreamMode::SpiEcho
            | UpstreamMode::SpiStatic
            | UpstreamMode::SpiLineHigh
    ) {
        spawner.spawn(
            spi_controller_task(
                p.SPI1,
                p.PIN_10,
                p.PIN_11,
                p.PIN_12,
                p.PIN_13,
                p.DMA_CH2,
                p.DMA_CH3,
                spawner,
                bridge_config,
                &LINK_ACTIVE,
                &SPI_FRAME_QUEUE,
                &SPI_RESPONSE_QUEUE,
            )
                .expect("spi controller task token allocation failed"),
        );
        Some(())
    } else {
        None
    };
    let mut upstream_usb_sender = None;
    let mut upstream_usb_receiver = None;
    let upstream_usb = if matches!(bridge_config.upstream_mode, UpstreamMode::Usb) {
        let driver = usb::Driver::new(p.USB, Irqs);
        let mut config = UsbConfig::new(0x2e8a, 0x000a);
        config.manufacturer = COMPILED_USB_DEVICE_NAMES.manufacturer;
        config.product = COMPILED_USB_DEVICE_NAMES.product;
        config.serial_number = COMPILED_USB_DEVICE_NAMES.serial_number;
        config.max_power = 100;
        config.max_packet_size_0 = 64;
        config.device_class = 0xEF;
        config.device_sub_class = 0x02;
        config.device_protocol = 0x01;
        config.composite_with_iads = true;

        let mut builder = UsbBuilder::new(
            driver,
            config,
            USB_CONFIG_DESC_BUF.init([0; 256]),
            USB_BOS_DESC_BUF.init([0; 256]),
            USB_MSOS_DESC_BUF.init([0; 256]),
            USB_CONTROL_BUF.init([0; 128]),
        );
        let state = USB_CDC_STATE.init(UsbCdcState::new());
        let class = CdcAcmClass::new(&mut builder, state, 64);
        let (sender, receiver) = class.split();
        let device = builder.build();
        upstream_usb_sender = Some(sender);
        upstream_usb_receiver = Some(receiver);
        spawner.spawn(usb_device_task(device).expect("usb device task token allocation failed"));
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

    if matches!(bridge_config.upstream_mode, UpstreamMode::Uart) {
        if let Some(uart) = uart.as_mut() {
            let _ = drain_uart_rx(uart, 10, 250).await;
        }
    }

    let result = run_bridge_mode(
        uart.as_mut(),
        stack,
        bridge_config,
        upstream_i2c.as_ref(),
        upstream_spi.as_ref(),
        upstream_usb.as_ref(),
        upstream_usb_sender.as_mut(),
        upstream_usb_receiver.as_mut(),
        status_led.as_mut(),
        &I2C_FRAME_QUEUE,
        &I2C_RESPONSE_QUEUE,
        &SPI_FRAME_QUEUE,
        &SPI_RESPONSE_QUEUE,
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
    upstream_usb_enabled: Option<&()>,
    usb_sender: Option<&mut UsbCdcSender>,
    usb_receiver: Option<&mut UsbCdcReceiver>,
    status_led: Option<&mut Output<'static>>,
    i2c_rx: &'static OverwriteQueue<I2cPacket, 8>,
    i2c_tx: &'static OverwriteQueue<I2cPacket, 8>,
    spi_rx: &'static OverwriteQueue<SpiFrame, 8>,
    spi_tx: &'static OverwriteQueue<SpiFrame, 8>,
) -> Result<(), ()> {
    let runtime = BridgeRuntime {
        link_active: &LINK_ACTIVE,
        startup_delay_ms: CLIENT_STARTUP_DELAY_MS,
        reconnect_delay_ms: CLIENT_RECONNECT_DELAY_MS,
        connect_timeout_ms: LINK_CONNECT_TIMEOUT_MS,
        handshake_timeout_ms: LINK_HANDSHAKE_TIMEOUT_MS,
        session_timeout_ms: LINK_SESSION_TIMEOUT_MS,
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
            Some(_) => {
                bridge::i2c::run_client(stack, host, port, bridge_config, runtime, i2c_rx, i2c_tx)
                    .await
            }
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::I2c) => match upstream_i2c_enabled {
            Some(_) => {
                bridge::i2c::run_server(stack, port, bridge_config, runtime, i2c_rx, i2c_tx).await
            }
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Usb) => {
            match (upstream_usb_enabled, usb_sender, usb_receiver) {
                (Some(_), Some(sender), Some(receiver)) => {
                    bridge::usb::run_client(
                        sender,
                        receiver,
                        stack,
                        host,
                        port,
                        bridge_config,
                        runtime,
                    )
                        .await
                }
                _ => Err(()),
            }
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Usb) => {
            match (upstream_usb_enabled, usb_sender, usb_receiver) {
                (Some(_), Some(sender), Some(receiver)) => {
                    bridge::usb::run_server(sender, receiver, stack, port, bridge_config, runtime)
                        .await
                }
                _ => Err(()),
            }
        }
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Spi) => match upstream_spi_enabled {
            Some(_) => {
                bridge::spi::run_client(
                    uart.expect("spi mode keeps boot UART for diagnostics"),
                    stack,
                    host,
                    port,
                    bridge_config,
                    runtime,
                    spi_rx,
                    spi_tx,
                )
                    .await
            }
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::Spi) => match upstream_spi_enabled {
            Some(_) => {
                bridge::spi::run_server(
                    uart.expect("spi mode keeps boot UART for diagnostics"),
                    stack,
                    port,
                    bridge_config,
                    runtime,
                    spi_rx,
                    spi_tx,
                )
                    .await
            }
            None => Err(()),
        },
        (_, UpstreamMode::SpiEcho | UpstreamMode::SpiStatic | UpstreamMode::SpiLineHigh) => loop {
            Timer::after_secs(1).await;
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
    tx: &'static OverwriteQueue<I2cPacket, 8>,
    rx_resp: &'static OverwriteQueue<I2cPacket, 8>,
) {
    i2c_poll_task(&mut i2c, bridge_config, link_active, tx, rx_resp).await
}

#[embassy_executor::task]
async fn spi_controller_task(
    spi1: embassy_rp::Peri<'static, SPI1>,
    sclk: embassy_rp::Peri<'static, PIN_10>,
    miso: embassy_rp::Peri<'static, PIN_11>,
    mosi: embassy_rp::Peri<'static, PIN_12>,
    cs: embassy_rp::Peri<'static, PIN_13>,
    tx_dma: embassy_rp::Peri<'static, DMA_CH2>,
    rx_dma: embassy_rp::Peri<'static, DMA_CH3>,
    spawner: Spawner,
    bridge_config: BridgeConfig,
    link_active: &'static AtomicBool,
    tx: &'static OverwriteQueue<SpiFrame, 8>,
    rx_resp: &'static OverwriteQueue<SpiFrame, 8>,
) {
    spi_poll_task(
        spi1,
        sclk,
        miso,
        mosi,
        cs,
        tx_dma,
        rx_dma,
        spawner,
        bridge_config,
        link_active,
        tx,
        rx_resp,
    )
        .await
}

/// Starts the Embassy executor and launches the async application task.
#[cortex_m_rt::entry]
fn main() -> ! {
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(app(spawner).expect("app task token allocation failed"));
    })
}
