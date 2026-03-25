#![no_std]
#![no_main]

mod config;

use config::{
    AddressMode, BridgeConfig, BridgeMode, Command, UpstreamMode, apply_command, parse_command,
    render_config,
};
use embassy_executor::{Executor, Spawner};
use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, PIN_10, PIN_11, PIN_12, PIN_13, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20,
    PIN_21, SPI0, SPI1, UART0,
};
use embassy_rp::spi::{self, Async, Blocking};
use embassy_rp::uart::{self, BufferedUart};
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use embedded_io_async::{Read, Write};
use heapless::{String, Vec};
#[allow(unused_imports)]
use panic_halt as _;
use portable_atomic::{AtomicBool, Ordering};
use static_cell::StaticCell;


type WizSpiDevice = ExclusiveDevice<spi::Spi<'static, SPI0, Async>, Output<'static>, Delay>;
const SPI_FRAME_SIZE: usize = 258;
const SPI_PAYLOAD_MAX: usize = SPI_FRAME_SIZE - 2;

struct UpstreamSpiDevice {
    _configured: spi::Spi<'static, SPI1, Blocking>,
    tx_frame: [u8; SPI_FRAME_SIZE],
    rx_frame: [u8; SPI_FRAME_SIZE],
    tx_idx: usize,
    rx_idx: usize,
}

type WizRunner = embassy_net_wiznet::Runner<
    'static,
    embassy_net_wiznet::chip::W5500,
    WizSpiDevice,
    Input<'static>,
    Output<'static>,
>;

bind_interrupts!(struct Irqs {
    UART0_IRQ => uart::BufferedInterruptHandler<UART0>;
});

static UART_TX_BUF: StaticCell<[u8; 512]> = StaticCell::new();
static UART_RX_BUF: StaticCell<[u8; 512]> = StaticCell::new();
static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
static WIZNET_STATE: StaticCell<embassy_net_wiznet::State<2, 2>> = StaticCell::new();
static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static LINK_ACTIVE: AtomicBool = AtomicBool::new(false);
const CLIENT_STARTUP_DELAY_MS: u64 = 250;
const CLIENT_RECONNECT_DELAY_MS: u64 = 500;
const LINK_CONNECT_TIMEOUT_MS: u64 = 1_500;
const LINK_HANDSHAKE_TIMEOUT_MS: u64 = 2_000;
const LINK_HANDSHAKE_MAGIC: &[u8] = b"PICOFI1";

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, embassy_net_wiznet::Device<'static>>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn wiz_task(runner: WizRunner) {
    runner.run().await;
}

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

    let _ = write_banner(&mut uart).await;
    let bridge_config = configuration_shell(&mut uart).await;
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
    let result = match (bridge_config.bridge_mode, bridge_config.upstream_mode) {
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Uart) => {
            run_client_uart_bridge(&mut uart, stack, host, port, bridge_config).await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Uart) => {
            run_server_uart_bridge(&mut uart, stack, port, bridge_config).await
        }
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Spi) => match upstream_spi.as_mut() {
            Some(spi) => run_client_spi_bridge(&mut uart, stack, host, port, spi, bridge_config).await,
            None => Err(()),
        },
        (BridgeMode::TcpServer { port }, UpstreamMode::Spi) => match upstream_spi.as_mut() {
            Some(spi) => run_server_spi_bridge(&mut uart, stack, port, spi, bridge_config).await,
            None => Err(()),
        },
        (BridgeMode::TcpClient { host, port }, UpstreamMode::Test) => {
            run_client_test_bridge(
                &mut uart,
                stack,
                host,
                port,
                status_led.as_mut().unwrap(),
            )
            .await
        }
        (BridgeMode::TcpServer { port }, UpstreamMode::Test) => {
            run_server_test_bridge(&mut uart, stack, port, status_led.as_mut().unwrap()).await
        }
    };

    if result.is_err() {
        let _ = writeln_line(&mut uart, "bridge stopped").await;
    }

    loop {
        Timer::after_secs(1).await;
    }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.must_spawn(app(spawner));
    })
}

async fn write_banner(uart: &mut BufferedUart) -> Result<(), ()> {
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
    writeln_line(uart, "  start").await
}

async fn configuration_shell(uart: &mut BufferedUart) -> BridgeConfig {
    let mut config = BridgeConfig::default();
    let rendered = render_config(&config);
    let _ = writeln_line(uart, "default:").await;
    let _ = writeln_line(uart, rendered.as_str()).await;
    let _ = writeln_line(uart, "auto-starting in 3 seconds; type commands to override or `start` now").await;

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

    let _ = writeln_line(uart, "starting compiled config").await;
    config
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

    let (device, wiz_runner) =
        embassy_net_wiznet::new::<2, 2, embassy_net_wiznet::chip::W5500, _, _, _>(
            bridge_config.mac_address,
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

async fn run_client_uart_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let _ = writeln_line(uart, "stabilizing before first connect").await;
    Timer::after_millis(CLIENT_STARTUP_DELAY_MS).await;
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "connecting").await;
        if connect_with_timeout(&mut socket, remote, port).await.is_err() {
            let _ = writeln_line(uart, "connect failed").await;
            Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
            continue;
        }
        let _ = writeln_line(uart, "tcp connected").await;
        if exchange_link_handshake(&mut socket, true).await.is_err() {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
            continue;
        }
        LINK_ACTIVE.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = uart_bridge_session(uart, &mut socket, bridge_config).await;
        socket.abort();
        let _ = socket.flush().await;
        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "server disconnected").await;
        let _ = writeln_line(uart, "cooling down before reconnect").await;
        Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
    }
}

async fn run_server_uart_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        let _ = writeln_line(uart, "waiting for tcp client").await;
        socket.accept(port).await.map_err(|_| ())?;
        let _ = writeln_line(uart, "tcp client connected").await;
        if exchange_link_handshake(&mut socket, false).await.is_err() {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            LINK_ACTIVE.store(false, Ordering::Relaxed);
            continue;
        }
        LINK_ACTIVE.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = uart_bridge_session(uart, &mut socket, bridge_config).await;
        socket.abort();
        let _ = socket.flush().await;
        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "client disconnected").await;
    }
}

async fn uart_bridge_session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    let mut uart_buf = [0u8; 256];
    let mut net_buf = [0u8; 256];
    let mut line_buf = String::<256>::new();

    loop {
        match select(uart.read(&mut uart_buf), socket.read(&mut net_buf)).await {
            Either::First(Ok(uart_n)) => {
                if uart_n == 0 {
                    Timer::after_millis(5).await;
                    continue;
                }
                for &byte in &uart_buf[..uart_n] {
                    match byte {
                        b'\r' => {}
                        b'\n' => {
                            if handle_local_bridge_command_uart(uart, bridge_config, line_buf.as_str()).await? {
                                line_buf.clear();
                                continue;
                            }
                            write_socket(socket, line_buf.as_bytes()).await?;
                            write_socket(socket, b"\n").await?;
                            line_buf.clear();
                        }
                        byte if byte.is_ascii() => {
                            let _ = line_buf.push(byte as char);
                        }
                        _ => {}
                    }
                }
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

fn init_upstream_spi(
    spi1: Peri<'static, SPI1>,
    sclk: Peri<'static, PIN_10>,
    mosi: Peri<'static, PIN_11>,
    miso: Peri<'static, PIN_12>,
    cs: Peri<'static, PIN_13>,
) -> UpstreamSpiDevice {
    let mut spi_config = spi::Config::default();
    spi_config.frequency = 1_000_000;
    let configured = spi::Spi::new_blocking(spi1, sclk, mosi, miso, spi_config);
    let _cs = cs;

    rp_pac::IO_BANK0.gpio(13).ctrl().write(|w| {
        w.set_funcsel(rp_pac::io::vals::Gpio13ctrlFuncsel::SPI1_SS_N.to_bits());
    });
    rp_pac::PADS_BANK0.gpio(13).modify(|w| {
        w.set_ie(true);
        w.set_pue(true);
        w.set_pde(false);
    });

    let p = rp_pac::SPI1;
    p.cr1().write_value(rp_pac::spi::regs::Cr1(0));
    p.cpsr().write_value({
        let mut reg = rp_pac::spi::regs::Cpsr(0);
        reg.set_cpsdvsr(2);
        reg
    });
    p.cr0().write_value({
        let mut w = rp_pac::spi::regs::Cr0(0);
        w.set_dss(0b0111);
        w.set_frf(0);
        w.set_spo(false);
        w.set_sph(false);
        w.set_scr(0);
        w
    });
    p.cr1().write_value({
        let mut w = rp_pac::spi::regs::Cr1(0);
        w.set_lbm(false);
        w.set_sse(false);
        w.set_ms(true);
        w.set_sod(false);
        w
    });
    p.dmacr().write_value({
        let mut w = rp_pac::spi::regs::Dmacr(0);
        w.set_rxdmae(false);
        w.set_txdmae(false);
        w
    });
    while p.sr().read().rne() {
        let _ = p.dr().read();
    }
    p.cr1().write_value({
        let mut w = rp_pac::spi::regs::Cr1(0);
        w.set_lbm(false);
        w.set_sse(true);
        w.set_ms(true);
        w.set_sod(false);
        w
    });

    let mut device = UpstreamSpiDevice {
        _configured: configured,
        tx_frame: [0; SPI_FRAME_SIZE],
        rx_frame: [0; SPI_FRAME_SIZE],
        tx_idx: 0,
        rx_idx: 0,
    };
    device.prepare_response_frame(&[]);
    device
}

async fn report_spi_probe(
    uart: &mut BufferedUart,
    spi: &mut UpstreamSpiDevice,
) -> Result<(), ()> {
    let bytes = spi_probe(spi)?;
    let line = render_hex_probe(&bytes);
    writeln_line(uart, line.as_str()).await
}

fn spi_probe(spi: &mut UpstreamSpiDevice) -> Result<[u8; 8], ()> {
    Ok(spi.rx_frame[..8].try_into().unwrap_or([0; 8]))
}

fn render_hex_probe(bytes: &[u8; 8]) -> String<48> {
    let mut out = String::<48>::new();
    let _ = out.push_str("spi probe=");
    for (idx, byte) in bytes.iter().enumerate() {
        if idx != 0 {
            let _ = out.push(' ');
        }
        push_hex_byte(&mut out, *byte);
    }
    out
}

fn push_hex_byte(out: &mut String<48>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}

async fn run_client_spi_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let _ = writeln_line(uart, "stabilizing before first connect").await;
    Timer::after_millis(CLIENT_STARTUP_DELAY_MS).await;
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "connecting").await;
        if connect_with_timeout(&mut socket, remote, port).await.is_err() {
            let _ = writeln_line(uart, "connect failed").await;
            Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
            continue;
        }
        let _ = writeln_line(uart, "tcp connected").await;
        if exchange_link_handshake(&mut socket, true).await.is_err() {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
            continue;
        }
        LINK_ACTIVE.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = spi_bridge_session(uart, &mut socket, spi, bridge_config).await;
        socket.abort();
        let _ = socket.flush().await;
        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "server disconnected").await;
        let _ = writeln_line(uart, "cooling down before reconnect").await;
        Timer::after_millis(CLIENT_RECONNECT_DELAY_MS).await;
    }
}

async fn run_server_spi_bridge(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        let _ = writeln_line(uart, "waiting for tcp client").await;
        socket.accept(port).await.map_err(|_| ())?;
        let _ = writeln_line(uart, "tcp client connected").await;
        if exchange_link_handshake(&mut socket, false).await.is_err() {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            LINK_ACTIVE.store(false, Ordering::Relaxed);
            continue;
        }
        LINK_ACTIVE.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = spi_bridge_session(uart, &mut socket, spi, bridge_config).await;
        socket.abort();
        let _ = socket.flush().await;
        LINK_ACTIVE.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "client disconnected").await;
    }
}

async fn run_client_test_bridge(
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

    let _ = writeln_line(uart, "connecting").await;
    socket.connect((remote, port)).await.map_err(|_| ())?;
    let _ = writeln_line(uart, "connected").await;

    test_bridge_session(uart, &mut socket, led).await
}

async fn run_server_test_bridge(
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

        let _ = writeln_line(uart, "waiting for tcp client").await;
        socket.accept(port).await.map_err(|_| ())?;
        let _ = writeln_line(uart, "client connected").await;

        let _ = test_bridge_session(uart, &mut socket, led).await;
        let _ = writeln_line(uart, "client disconnected").await;
    }
}

async fn spi_bridge_session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
) -> Result<(), ()> {
    let _ = writeln_line(
        uart,
        "spi slave upstream enabled on SPI1 pins: sck=10 mosi=11 miso=12 cs=13",
    )
    .await;
    let mut net_buf = [0u8; SPI_PAYLOAD_MAX];
    let mut line_buf = String::<256>::new();
    spi.prepare_response_frame(&[]);

    loop {
        if !socket.may_recv() && !socket.can_recv() {
            return Ok(());
        }
        if socket.recv_queue() > 0 {
            let net_n = socket.read(&mut net_buf).await.map_err(|_| ())?;
            if net_n == 0 {
                return Ok(());
            }
            spi.prepare_response_frame(&net_buf[..net_n.min(SPI_PAYLOAD_MAX)]);
        }

        if let Some(frame) = spi.poll_transaction() {
            if let Some(payload) = parse_spi_request_frame(frame) {
                if !payload.is_empty() {
                    let mut inbound = Vec::<u8, SPI_PAYLOAD_MAX>::new();
                    let _ = inbound.extend_from_slice(payload);
                    for &byte in inbound.iter() {
                        match byte {
                            b'\r' => {}
                            b'\n' => {
                                if line_buf.starts_with('/') {
                                    let response = render_local_bridge_command(bridge_config, line_buf.as_str());
                                    spi.prepare_response_frame(response.as_bytes());
                                } else {
                                    write_socket(socket, line_buf.as_bytes()).await?;
                                    write_socket(socket, b"\n").await?;
                                }
                                line_buf.clear();
                            }
                            byte if byte.is_ascii() => {
                                let _ = line_buf.push(byte as char);
                            }
                            _ => {}
                        }
                    }
                }
            }
            if socket.recv_queue() == 0 {
                spi.prepare_response_frame(&[]);
            }
        }
        Timer::after_millis(1).await;
    }
}

async fn exchange_link_handshake(socket: &mut TcpSocket<'_>, initiator: bool) -> Result<(), ()> {
    let mut buf = [0u8; 7];

    if initiator {
        write_socket(socket, LINK_HANDSHAKE_MAGIC).await?;
        match select(
            read_socket_exact(socket, &mut buf),
            Timer::after_millis(LINK_HANDSHAKE_TIMEOUT_MS),
        )
        .await
        {
            Either::First(Ok(())) if buf == LINK_HANDSHAKE_MAGIC => Ok(()),
            _ => Err(()),
        }
    } else {
        match select(
            read_socket_exact(socket, &mut buf),
            Timer::after_millis(LINK_HANDSHAKE_TIMEOUT_MS),
        )
        .await
        {
            Either::First(Ok(())) if buf == LINK_HANDSHAKE_MAGIC => {
                write_socket(socket, LINK_HANDSHAKE_MAGIC).await
            }
            _ => Err(()),
        }
    }
}

async fn handle_local_bridge_command_uart(
    uart: &mut BufferedUart,
    bridge_config: BridgeConfig,
    line: &str,
) -> Result<bool, ()> {
    if !line.starts_with('/') {
        return Ok(false);
    }
    let response = render_local_bridge_command(bridge_config, line);
    writeln_line(uart, response.as_str()).await?;
    Ok(true)
}

fn render_local_bridge_command(bridge_config: BridgeConfig, line: &str) -> String<192> {
    let mut out = String::<192>::new();
    match line {
        "/help" => {
            let _ = out.push_str("pico commands: /help /show /ping /link");
        }
        "/show" => {
            let rendered = render_config(&bridge_config);
            let _ = out.push_str(rendered.as_str());
        }
        "/ping" => {
            let _ = out.push_str("pong");
        }
        "/link" => {
            if LINK_ACTIVE.load(Ordering::Relaxed) {
                let _ = out.push_str("link up");
            } else {
                let _ = out.push_str("link down");
            }
        }
        _ => {
            let _ = out.push_str("error unknown pico command");
        }
    }
    out
}

async fn read_socket_exact(socket: &mut TcpSocket<'_>, mut buf: &mut [u8]) -> Result<(), ()> {
    while !buf.is_empty() {
        match socket.read(buf).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => buf = &mut buf[n..],
        }
    }
    Ok(())
}

async fn connect_with_timeout(
    socket: &mut TcpSocket<'_>,
    remote: Ipv4Address,
    port: u16,
) -> Result<(), ()> {
    match select(
        socket.connect((remote, port)),
        Timer::after_millis(LINK_CONNECT_TIMEOUT_MS),
    )
    .await
    {
        Either::First(Ok(())) => Ok(()),
        Either::First(Err(_)) | Either::Second(()) => {
            socket.abort();
            Err(())
        }
    }
}

impl UpstreamSpiDevice {
    fn prepare_response_frame(&mut self, payload: &[u8]) {
        self.reset_spi_state();
        self.tx_frame.fill(0);
        self.rx_frame.fill(0);
        self.tx_frame[0] = 0x5a;
        let len = payload.len().min(SPI_PAYLOAD_MAX);
        self.tx_frame[1] = len as u8;
        self.tx_frame[2..2 + len].copy_from_slice(&payload[..len]);
        self.tx_idx = 0;
        self.rx_idx = 0;
        self.prime_tx_fifo();
    }

    fn reset_spi_state(&mut self) {
        let p = rp_pac::SPI1;
        p.cr1().modify(|w| w.set_sse(false));
        while p.sr().read().rne() {
            let _ = p.dr().read();
        }
        p.icr().write_value({
            let mut w = rp_pac::spi::regs::Icr(0);
            w.set_roric(true);
            w.set_rtic(true);
            w
        });
        p.cr1().modify(|w| w.set_sse(true));
    }

    fn prime_tx_fifo(&mut self) {
        let p = rp_pac::SPI1;
        while self.tx_idx < SPI_FRAME_SIZE && p.sr().read().tnf() {
            p.dr().write_value({
                let mut w = rp_pac::spi::regs::Dr(0);
                w.set_data(self.tx_frame[self.tx_idx] as u16);
                w
            });
            self.tx_idx += 1;
        }
    }

    fn poll_transaction(&mut self) -> Option<&[u8; SPI_FRAME_SIZE]> {
        let p = rp_pac::SPI1;
        if self.rx_idx > 0 && self.rx_idx < SPI_FRAME_SIZE && !p.sr().read().bsy() && !p.sr().read().rne() {
            // The master released CS before a full framed exchange completed.
            // Reset to the beginning of the canned response so the next transaction starts cleanly.
            self.prepare_response_frame(&[]);
        }
        self.prime_tx_fifo();
        while p.sr().read().rne() {
            let byte = p.dr().read().data() as u8;
            if self.rx_idx < SPI_FRAME_SIZE {
                self.rx_frame[self.rx_idx] = byte;
                self.rx_idx += 1;
            }
            self.prime_tx_fifo();
            if self.rx_idx == SPI_FRAME_SIZE {
                return Some(&self.rx_frame);
            }
        }
        None
    }
}

fn parse_spi_request_frame(frame: &[u8; SPI_FRAME_SIZE]) -> Option<&[u8]> {
    if frame[0] != 0xa5 {
        return None;
    }
    let len = frame[1] as usize;
    if len > SPI_PAYLOAD_MAX {
        return None;
    }
    Some(&frame[2..2 + len])
}

async fn test_bridge_session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    led: &mut Output<'static>,
) -> Result<(), ()> {
    let _ = writeln_line(uart, "test mode ready").await;
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
        let response = handle_test_command(line, led, &mut led_on).await;
        write_socket(socket, response.as_bytes()).await?;
        write_socket(socket, b"\r\n").await?;
        let _ = writeln_line(uart, response).await;
    }
}

async fn handle_test_command<'a>(
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
            if *led_on { "led on" } else { "led off" }
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

fn trim_ascii_line(buf: &[u8]) -> &str {
    let text = core::str::from_utf8(buf).unwrap_or_default();
    text.trim_matches(|ch| matches!(ch, '\r' | '\n' | ' ' | '\t'))
}

fn parse_blink_command(line: &str) -> Option<u64> {
    let value = line.strip_prefix("led blink ")?;
    value.parse::<u64>().ok()
}

async fn read_line_with_timeout(
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
