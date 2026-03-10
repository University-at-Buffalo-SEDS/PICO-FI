#![no_std]
#![no_main]

mod config;

use config::{
    AddressMode, BridgeConfig, BridgeMode, Command, apply_command, parse_command, render_config,
};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, SPI0, UART0,
};
use embassy_rp::spi::{self, Async};
use embassy_rp::uart::{self, BufferedUart};
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io_async::{Read, Write};
use heapless::{String, Vec};
use panic_halt as _;
use static_cell::StaticCell;

type WizSpiDevice = ExclusiveDevice<spi::Spi<'static, SPI0, Async>, Output<'static>, Delay>;
type WizRunner = embassy_net_wiznet::Runner<
    'static,
    embassy_net_wiznet::chip::W5500,
    WizSpiDevice,
    Input<'static>,
    Output<'static>,
>;

#[unsafe(link_section = ".boot2")]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_GENERIC_03H;

bind_interrupts!(struct Irqs {
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
});

static UART_TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();
static UART_RX_BUF: StaticCell<[u8; 512]> = StaticCell::new();
static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
static WIZNET_STATE: StaticCell<embassy_net_wiznet::State<2, 2>> = StaticCell::new();

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, embassy_net_wiznet::Device<'static>>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn wiz_task(runner: WizRunner) {
    runner.run().await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

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

    let _ = write_banner(&mut uart).await;
    let bridge_config = configuration_shell(&mut uart).await;

    let stack = match bring_up_network(
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
    let result = match bridge_config.bridge_mode {
        BridgeMode::TcpClient { host, port } => {
            run_client_bridge(&mut uart, stack, host, port).await
        }
        BridgeMode::TcpServer { port } => run_server_bridge(&mut uart, stack, port).await,
    };

    if result.is_err() {
        let _ = writeln_line(&mut uart, "bridge stopped").await;
    }

    loop {
        Timer::after_secs(1).await;
    }
}

async fn write_banner(uart: &mut BufferedUart) -> Result<(), ()> {
    writeln_line(uart, "").await?;
    writeln_line(uart, "pico-fi uart bridge").await?;
    writeln_line(uart, "commands:").await?;
    writeln_line(uart, "  show").await?;
    writeln_line(uart, "  set dhcp").await?;
    writeln_line(uart, "  set static <ip>/<prefix> <gateway> <dns>").await?;
    writeln_line(uart, "  set client <dest-ip> <port>").await?;
    writeln_line(uart, "  set server <listen-port>").await?;
    writeln_line(uart, "  start").await
}

async fn configuration_shell(uart: &mut BufferedUart) -> BridgeConfig {
    let mut config = BridgeConfig::default();
    let _ = writeln_line(uart, "default: ip=dhcp mode=server listen=5000").await;

    loop {
        let _ = write_str(uart, "> ").await;
        let mut line = String::<128>::new();
        if read_line(uart, &mut line).await.is_err() {
            let _ = writeln_line(uart, "uart read error").await;
            continue;
        }

        match parse_command(line.as_str()) {
            Ok(Command::Help) => {
                let _ = write_banner(uart).await;
            }
            Ok(Command::Show) => {
                let rendered = render_config(&config);
                let _ = writeln_line(uart, rendered.as_str()).await;
            }
            Ok(command) => {
                if apply_command(&mut config, command) {
                    let rendered = render_config(&config);
                    let _ = writeln_line(uart, rendered.as_str()).await;
                    return config;
                }
                let rendered = render_config(&config);
                let _ = writeln_line(uart, rendered.as_str()).await;
            }
            Err(err) => {
                let _ = writeln_line(uart, err).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn bring_up_network(
    spawner: Spawner,
    spi0: Peri<'static, SPI0>,
    miso: Peri<'static, PIN_16>,
    cs: Peri<'static, PIN_17>,
    sclk: Peri<'static, PIN_18>,
    mosi: Peri<'static, PIN_19>,
    reset: Peri<'static, PIN_20>,
    int: Peri<'static, PIN_21>,
    tx_dma: Peri<'static, DMA_CH0>,
    rx_dma: Peri<'static, DMA_CH1>,
    bridge_config: BridgeConfig,
) -> Result<Stack<'static>, &'static str> {
    let mut spi_config = spi::Config::default();
    spi_config.frequency = 30_000_000;

    let spi = spi::Spi::new(spi0, sclk, mosi, miso, tx_dma, rx_dma, spi_config);
    let cs = Output::new(cs, Level::High);
    let reset = Output::new(reset, Level::High);
    let int = Input::new(int, Pull::Up);
    let spi_dev = ExclusiveDevice::new(spi, cs, Delay).map_err(|_| "spi device init failed")?;

    let mac = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
    let (device, wiz_runner) =
        embassy_net_wiznet::new::<2, 2, embassy_net_wiznet::chip::W5500, _, _, _>(
            mac,
            WIZNET_STATE.init(embassy_net_wiznet::State::new()),
            spi_dev,
            int,
            reset,
        )
        .await
        .map_err(|_| "w5500 init failed")?;

    let net_config = match bridge_config.address_mode {
        AddressMode::Dhcp => NetConfig::dhcpv4(Default::default()),
        AddressMode::Static(static_ip) => {
            let mut dns_servers = Vec::<Ipv4Address, 3>::new();
            dns_servers
                .push(Ipv4Address::new(
                    static_ip.dns[0],
                    static_ip.dns[1],
                    static_ip.dns[2],
                    static_ip.dns[3],
                ))
                .map_err(|_| "dns config failed")?;

            NetConfig::ipv4_static(StaticConfigV4 {
                address: Ipv4Cidr::new(
                    Ipv4Address::new(
                        static_ip.address[0],
                        static_ip.address[1],
                        static_ip.address[2],
                        static_ip.address[3],
                    ),
                    static_ip.prefix_len,
                ),
                gateway: Some(Ipv4Address::new(
                    static_ip.gateway[0],
                    static_ip.gateway[1],
                    static_ip.gateway[2],
                    static_ip.gateway[3],
                )),
                dns_servers,
            })
        }
    };

    let seed = 0x0012_3456_89ab_cdef;
    let (stack, net_runner) = embassy_net::new(
        device,
        net_config,
        NET_RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner
        .spawn(wiz_task(wiz_runner))
        .map_err(|_| "wiz task spawn failed")?;
    spawner
        .spawn(net_task(net_runner))
        .map_err(|_| "net task spawn failed")?;

    while !stack.is_link_up() {
        Timer::after_millis(250).await;
    }

    if matches!(bridge_config.address_mode, AddressMode::Dhcp) {
        while stack.config_v4().is_none() {
            Timer::after_millis(250).await;
        }
    }

    Ok(stack)
}

async fn run_client_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(Some(Duration::from_secs(30)));

    let _ = writeln_line(uart, "connecting").await;
    socket.connect((remote, port)).await.map_err(|_| ())?;
    let _ = writeln_line(uart, "connected").await;

    bridge_session(uart, &mut socket).await
}

async fn run_server_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
) -> Result<(), ()> {
    let mut rx_buf = [0u8; 2048];
    let mut tx_buf = [0u8; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
    socket.set_timeout(None);

    let _ = writeln_line(uart, "waiting for tcp client").await;
    socket.accept(port).await.map_err(|_| ())?;
    let _ = writeln_line(uart, "client connected").await;

    bridge_session(uart, &mut socket).await
}

async fn bridge_session(uart: &mut BufferedUart, socket: &mut TcpSocket<'_>) -> Result<(), ()> {
    let mut uart_buf = [0u8; 256];
    let mut net_buf = [0u8; 256];

    loop {
        match select(uart.read(&mut uart_buf), socket.read(&mut net_buf)).await {
            Either::First(Ok(uart_n)) => {
                if uart_n == 0 {
                    Timer::after_millis(5).await;
                    continue;
                }
                write_socket(socket, &uart_buf[..uart_n]).await?;
            }
            Either::First(Err(_)) => return Err(()),
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                uart.write_all(&net_buf[..net_n]).await.map_err(|_| ())?;
                uart.flush().await.map_err(|_| ())?;
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

async fn read_line(uart: &mut BufferedUart, line: &mut String<128>) -> Result<(), ()> {
    let mut byte = [0u8; 1];

    loop {
        uart.read_exact(&mut byte).await.map_err(|_| ())?;
        match byte[0] {
            b'\r' | b'\n' => {
                let _ = writeln_line(uart, "").await;
                return Ok(());
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
        }
    }
}

async fn write_str(uart: &mut BufferedUart, value: &str) -> Result<(), ()> {
    uart.write_all(value.as_bytes()).await.map_err(|_| ())?;
    uart.flush().await.map_err(|_| ())
}

async fn writeln_line(uart: &mut BufferedUart, value: &str) -> Result<(), ()> {
    uart.write_all(value.as_bytes()).await.map_err(|_| ())?;
    uart.write_all(b"\r\n").await.map_err(|_| ())?;
    uart.flush().await.map_err(|_| ())
}

async fn write_socket(socket: &mut TcpSocket<'_>, mut buf: &[u8]) -> Result<(), ()> {
    while !buf.is_empty() {
        let written = socket.write(buf).await.map_err(|_| ())?;
        if written == 0 {
            return Err(());
        }
        buf = &buf[written..];
    }
    Ok(())
}
