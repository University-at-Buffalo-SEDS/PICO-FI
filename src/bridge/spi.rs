//! SPI upstream bridge implementation and SPI slave framing state.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::spi::{
    FRAME_SIZE, PAYLOAD_MAX, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, parse_request_frame,
};
use crate::shell::writeln_line;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::Peri;
use embassy_rp::peripherals::{PIN_10, PIN_11, PIN_12, PIN_13, SPI1};
use embassy_rp::spi::{self, Blocking};
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use heapless::String;
use portable_atomic::{AtomicBool, Ordering};

/// Stateful SPI slave transport that buffers one request and one response frame.
pub struct UpstreamSpiDevice {
    /// Keeps the Embassy SPI peripheral configured for the lifetime of the slave device.
    _configured: spi::Spi<'static, SPI1, Blocking>,
    /// Contains the next response frame to transmit back to the SPI master.
    tx_frame: [u8; FRAME_SIZE],
    /// Captures the most recent request frame received from the SPI master.
    rx_frame: [u8; FRAME_SIZE],
    /// Tracks how many response bytes have been queued into the SPI TX FIFO.
    tx_idx: usize,
    /// Tracks how many request bytes have been received in the active transaction.
    rx_idx: usize,
    /// Indicates whether chip-select is currently asserted.
    cs_active: bool,
    /// Clears a one-shot response payload after the master finishes reading it.
    clear_after_transaction: bool,
}

/// Configures `SPI1` as the framed upstream slave transport.
pub fn init_upstream_spi(
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
        tx_frame: [0; FRAME_SIZE],
        rx_frame: [0; FRAME_SIZE],
        tx_idx: 0,
        rx_idx: 0,
        cs_active: false,
        clear_after_transaction: false,
    };
    device.prepare_response_frame(RESP_DATA_MAGIC, &[]);
    device
}

/// Emits a short SPI probe line onto UART for debugging transport health.
pub async fn report_spi_probe(
    uart: &mut BufferedUart,
    spi: &mut UpstreamSpiDevice,
) -> Result<(), ()> {
    let bytes = spi_probe(spi)?;
    let line = render_hex_probe(&bytes);
    writeln_line(uart, line.as_str()).await
}

/// Runs the SPI bridge in TCP client mode with reconnect behavior.
pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    startup_delay_ms: u64,
    reconnect_delay_ms: u64,
    connect_timeout_ms: u64,
    handshake_timeout_ms: u64,
    handshake_magic: &[u8],
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    let _ = writeln_line(uart, "stabilizing before first connect").await;
    Timer::after_millis(startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "connecting").await;
        if connect_with_timeout(&mut socket, remote, port, connect_timeout_ms)
            .await
            .is_err()
        {
            let _ = writeln_line(uart, "connect failed").await;
            Timer::after_millis(reconnect_delay_ms).await;
            continue;
        }
        let _ = writeln_line(uart, "tcp connected").await;
        if exchange_link_handshake(&mut socket, true, handshake_magic, handshake_timeout_ms)
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            Timer::after_millis(reconnect_delay_ms).await;
            continue;
        }
        link_active.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = session(uart, &mut socket, spi, bridge_config, link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "server disconnected").await;
        let _ = writeln_line(uart, "cooling down before reconnect").await;
        Timer::after_millis(reconnect_delay_ms).await;
    }
}

/// Runs the SPI bridge in TCP server mode.
pub async fn run_server(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    handshake_timeout_ms: u64,
    handshake_magic: &[u8],
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        let _ = writeln_line(uart, "waiting for tcp client").await;
        socket.accept(port).await.map_err(|_| ())?;
        let _ = writeln_line(uart, "tcp client connected").await;
        if exchange_link_handshake(&mut socket, false, handshake_magic, handshake_timeout_ms)
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            let _ = writeln_line(uart, "handshake failed").await;
            link_active.store(false, Ordering::Relaxed);
            continue;
        }
        link_active.store(true, Ordering::Relaxed);
        let _ = writeln_line(uart, "link active").await;

        let _ = session(uart, &mut socket, spi, bridge_config, link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        link_active.store(false, Ordering::Relaxed);
        let _ = writeln_line(uart, "client disconnected").await;
    }
}

/// Bridges framed SPI transactions to the TCP socket.
async fn session(
    uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
    let _ = writeln_line(
        uart,
        "spi slave upstream enabled on SPI1 pins: sck=10 mosi=11 miso=12 cs=13",
    )
    .await;
    let mut net_buf = [0u8; PAYLOAD_MAX];
    spi.prepare_response_frame(RESP_DATA_MAGIC, &[]);

    loop {
        if !socket.may_recv() && !socket.can_recv() {
            return Ok(());
        }
        if socket.recv_queue() > 0 {
            let net_n = socket.read(&mut net_buf).await.map_err(|_| ())?;
            if net_n == 0 {
                return Ok(());
            }
            spi.prepare_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n.min(PAYLOAD_MAX)]);
        }

        if let Some(frame) = spi.poll_transaction() {
            match parse_request_frame(frame) {
                Some(RequestFrame::Data(payload)) if !payload.is_empty() => {
                    write_socket(socket, payload).await?;
                }
                Some(RequestFrame::Command(payload)) => {
                    let response = render_local_bridge_command(
                        bridge_config,
                        link_active,
                        trim_ascii_line(payload),
                    );
                    spi.prepare_response_frame(RESP_COMMAND_MAGIC, response.as_bytes());
                }
                _ => {}
            }
        }
        Timer::after_millis(1).await;
    }
}

impl UpstreamSpiDevice {
    /// Stages a response frame for the next SPI transaction.
    fn prepare_response_frame(&mut self, magic: u8, payload: &[u8]) {
        self.clear_after_transaction = !payload.is_empty();
        self.tx_frame.fill(0);
        self.tx_frame[0] = magic;
        let len = payload.len().min(PAYLOAD_MAX);
        self.tx_frame[1] = len as u8;
        self.tx_frame[2..2 + len].copy_from_slice(&payload[..len]);
        if !self.cs_active {
            self.begin_transaction();
        }
    }

    /// Reinitializes the RX/TX state for the next CS-bounded transaction.
    fn begin_transaction(&mut self) {
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
        self.rx_frame.fill(0);
        self.tx_idx = 0;
        self.rx_idx = 0;
        self.prime_tx_fifo();
    }

    /// Samples the hardware chip-select line to detect transaction edges.
    fn cs_is_low(&self) -> bool {
        !rp_pac::IO_BANK0.gpio(13).status().read().infrompad()
    }

    /// Fills the hardware TX FIFO with as much of the response frame as possible.
    fn prime_tx_fifo(&mut self) {
        let p = rp_pac::SPI1;
        while self.tx_idx < FRAME_SIZE && p.sr().read().tnf() {
            p.dr().write_value({
                let mut w = rp_pac::spi::regs::Dr(0);
                w.set_data(self.tx_frame[self.tx_idx] as u16);
                w
            });
            self.tx_idx += 1;
        }
    }

    /// Polls the active SPI transaction and yields a complete frame once fully received.
    fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        let p = rp_pac::SPI1;
        let cs_low = self.cs_is_low();

        if cs_low && !self.cs_active {
            self.cs_active = true;
        } else if !cs_low && self.cs_active {
            self.cs_active = false;
            if self.clear_after_transaction {
                self.clear_after_transaction = false;
                self.tx_frame.fill(0);
                self.tx_frame[0] = RESP_DATA_MAGIC;
                self.tx_frame[1] = 0;
            }
            self.begin_transaction();
            return None;
        }

        if !self.cs_active {
            return None;
        }

        self.prime_tx_fifo();
        while p.sr().read().rne() {
            let byte = p.dr().read().data() as u8;
            if self.rx_idx < FRAME_SIZE {
                self.rx_frame[self.rx_idx] = byte;
                self.rx_idx += 1;
            }
            self.prime_tx_fifo();
            if self.rx_idx == FRAME_SIZE {
                return Some(&self.rx_frame);
            }
        }
        None
    }
}

/// Reads the last captured SPI bytes for a minimal probe display.
fn spi_probe(spi: &mut UpstreamSpiDevice) -> Result<[u8; 8], ()> {
    Ok(spi.rx_frame[..8].try_into().unwrap_or([0; 8]))
}

/// Formats probe bytes as a compact hexadecimal status line.
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

/// Appends a two-character uppercase hexadecimal byte to a string buffer.
fn push_hex_byte(out: &mut String<48>, value: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let _ = out.push(HEX[(value >> 4) as usize] as char);
    let _ = out.push(HEX[(value & 0x0f) as usize] as char);
}
