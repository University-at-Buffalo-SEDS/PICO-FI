//! Ethernet bring-up and TCP transport helpers.

use crate::Irqs;
use crate::config::{AddressMode, BridgeConfig};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::tcp::TcpSocket;
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_rp::Peri;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, SPI0,
};
use embassy_rp::spi::{self, Async};
use embassy_time::{Delay, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::Vec;
use static_cell::StaticCell;

/// Alias for the shared W5500 SPI device wrapper.
pub type WizSpiDevice = ExclusiveDevice<spi::Spi<'static, SPI0, Async>, Output<'static>, Delay>;

/// Embassy runner type for the W5500 driver task.
pub type WizRunner = embassy_net_wiznet::Runner<
    'static,
    embassy_net_wiznet::chip::W5500,
    WizSpiDevice,
    Input<'static>,
    Output<'static>,
>;

/// Shared network stack resources used by Embassy.
static NET_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

/// Shared W5500 driver state.
static WIZNET_STATE: StaticCell<embassy_net_wiznet::State<2, 2>> = StaticCell::new();

/// Number of bytes in the Ethernet bridge frame header.
pub const BRIDGE_FRAME_HEADER_SIZE: usize = 4;
const BRIDGE_FRAME_INLINE_WRITE_MAX: usize = 512;

const BRIDGE_FRAME_MAGIC0: u8 = 0xB5;
const BRIDGE_FRAME_MAGIC1: u8 = 0x4E;

/// Runs the background Embassy network stack task.
#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, embassy_net_wiznet::Device<'static>>) {
    runner.run().await;
}

/// Runs the background W5500 driver task.
#[embassy_executor::task]
pub async fn wiz_task(runner: WizRunner) {
    runner.run().await;
}

/// Initializes the W5500 network stack and waits for link/address readiness.
#[allow(clippy::too_many_arguments)]
pub async fn bring_up_network(
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

    let spi = spi::Spi::new(spi0, sclk, mosi, miso, tx_dma, rx_dma, Irqs, spi_config);
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

    spawner.spawn(wiz_task(wiz_runner).map_err(|_| "wiz task spawn failed")?);
    spawner.spawn(net_task(net_runner).map_err(|_| "net task spawn failed")?);

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

/// Performs the fixed magic handshake that marks a bridged TCP socket as usable.
pub async fn exchange_link_handshake(
    socket: &mut TcpSocket<'_>,
    initiator: bool,
    handshake_magic: &[u8],
    timeout_ms: u64,
) -> Result<(), ()> {
    let mut buf = [0u8; 7];

    if initiator {
        write_socket(socket, handshake_magic).await?;
        match select(
            read_socket_exact(socket, &mut buf),
            Timer::after_millis(timeout_ms),
        )
        .await
        {
            Either::First(Ok(())) if buf == handshake_magic => Ok(()),
            _ => Err(()),
        }
    } else {
        match select(
            read_socket_exact(socket, &mut buf),
            Timer::after_millis(timeout_ms),
        )
        .await
        {
            Either::First(Ok(())) if buf == handshake_magic => {
                write_socket(socket, handshake_magic).await
            }
            _ => Err(()),
        }
    }
}

/// Connects a TCP socket with a timeout and aborts it if the timeout expires.
pub async fn connect_with_timeout(
    socket: &mut TcpSocket<'_>,
    remote: Ipv4Address,
    port: u16,
    timeout_ms: u64,
) -> Result<(), ()> {
    match select(
        socket.connect((remote, port)),
        Timer::after_millis(timeout_ms),
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

/// Reads exactly `buf.len()` bytes from a socket unless the socket closes first.
pub async fn read_socket_exact(socket: &mut TcpSocket<'_>, mut buf: &mut [u8]) -> Result<(), ()> {
    while !buf.is_empty() {
        match socket.read(buf).await {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => buf = &mut buf[n..],
        }
    }
    Ok(())
}

/// Writes the entire provided buffer to the socket.
pub async fn write_socket(socket: &mut TcpSocket<'_>, mut buf: &[u8]) -> Result<(), ()> {
    while !buf.is_empty() {
        let written = socket.write(buf).await.map_err(|_| ())?;
        if written == 0 {
            return Err(());
        }
        buf = &buf[written..];
    }
    Ok(())
}

/// Writes one length-prefixed bridge payload to the TCP stream.
pub async fn write_bridge_frame(socket: &mut TcpSocket<'_>, payload: &[u8]) -> Result<(), ()> {
    if payload.len() > u16::MAX as usize {
        return Err(());
    }

    let mut header = [0u8; BRIDGE_FRAME_HEADER_SIZE];
    header[0] = BRIDGE_FRAME_MAGIC0;
    header[1] = BRIDGE_FRAME_MAGIC1;
    header[2..4].copy_from_slice(&(payload.len() as u16).to_le_bytes());

    // Keep the common small-packet case in one TCP write to minimize
    // latency from extra segmentation on the Ethernet leg.
    if payload.len() + BRIDGE_FRAME_HEADER_SIZE <= BRIDGE_FRAME_INLINE_WRITE_MAX {
        let mut frame = [0u8; BRIDGE_FRAME_INLINE_WRITE_MAX];
        let frame_len = BRIDGE_FRAME_HEADER_SIZE + payload.len();
        frame[..BRIDGE_FRAME_HEADER_SIZE].copy_from_slice(&header);
        frame[BRIDGE_FRAME_HEADER_SIZE..frame_len].copy_from_slice(payload);
        return write_socket(socket, &frame[..frame_len]).await;
    }

    write_socket(socket, &header).await?;
    write_socket(socket, payload).await
}

/// Reads one length-prefixed bridge payload from the TCP stream into `buf`.
pub async fn read_bridge_frame(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8],
) -> Result<usize, ()> {
    let mut header = [0u8; BRIDGE_FRAME_HEADER_SIZE];
    read_socket_exact(socket, &mut header).await?;
    if header[0] != BRIDGE_FRAME_MAGIC0 || header[1] != BRIDGE_FRAME_MAGIC1 {
        return Err(());
    }

    let len = u16::from_le_bytes([header[2], header[3]]) as usize;
    if len > buf.len() {
        return Err(());
    }

    read_socket_exact(socket, &mut buf[..len]).await?;
    Ok(len)
}
