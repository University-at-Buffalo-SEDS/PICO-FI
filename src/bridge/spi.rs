//! SPI upstream bridge implementation and SPI slave framing state.

use core::sync::atomic::{Ordering as MemoryOrdering, compiler_fence};

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::spi::{
    FRAME_SIZE, PAYLOAD_MAX, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, parse_request_frame,
};
use embassy_futures::yield_now;
use embassy_rp::dma::ChannelInstance;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::Peri;
use embassy_rp::peripherals::{PIN_10, PIN_11, PIN_12, PIN_13, SPI1};
use embassy_rp::spi::{self, Blocking};
use embassy_rp::uart::BufferedUart;
use embassy_time::{Duration, Timer};
use rp_pac::dma::vals::{DataSize, TreqSel};
use portable_atomic::{AtomicBool, Ordering};

/// Stateful SPI slave transport that buffers one request and one response frame.
pub struct UpstreamSpiDevice {
    /// Keeps the Embassy SPI peripheral configured for the lifetime of the slave device.
    _configured: spi::Spi<'static, SPI1, Blocking>,
    /// DMA channel number used to feed response bytes into the SPI TX FIFO.
    tx_dma_ch: u8,
    /// DMA channel number used to drain request bytes from the SPI RX FIFO.
    rx_dma_ch: u8,
    /// Contains the next response frame to transmit back to the SPI master.
    tx_frame: [u8; FRAME_SIZE],
    /// Captures the most recent request frame received from the SPI master.
    rx_frame: [u8; FRAME_SIZE],
    /// Indicates whether chip-select is currently asserted.
    cs_active: bool,
    /// Tracks whether the current transaction's frame has already been surfaced to the caller.
    frame_reported: bool,
    /// Clears a one-shot response payload after the master finishes reading it.
    clear_after_transaction: bool,
}

/// Configures `SPI1` as the framed upstream slave transport.
pub fn init_upstream_spi<TX: ChannelInstance, RX: ChannelInstance>(
    spi1: Peri<'static, SPI1>,
    sclk: Peri<'static, PIN_10>,
    mosi: Peri<'static, PIN_11>,
    miso: Peri<'static, PIN_12>,
    cs: Peri<'static, PIN_13>,
    _tx_dma: Peri<'static, TX>,
    _rx_dma: Peri<'static, RX>,
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
        tx_dma_ch: TX::number(),
        rx_dma_ch: RX::number(),
        tx_frame: [0; FRAME_SIZE],
        rx_frame: [0; FRAME_SIZE],
        cs_active: false,
        frame_reported: false,
        clear_after_transaction: false,
    };
    device.prepare_response_frame(RESP_DATA_MAGIC, &[]);
    device
}

/// Runs the SPI bridge in TCP client mode with reconnect behavior.
pub async fn run_client(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    let remote = Ipv4Address::new(host[0], host[1], host[2], host[3]);
    Timer::after_millis(runtime.startup_delay_ms).await;

    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        runtime.link_active.store(false, Ordering::Relaxed);
        if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms)
            .await
            .is_err()
        {
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
        if exchange_link_handshake(
            &mut socket,
            true,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            Timer::after_millis(runtime.reconnect_delay_ms).await;
            continue;
        }
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(uart, &mut socket, spi, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}

/// Runs the SPI bridge in TCP server mode.
pub async fn run_server(
    uart: &mut BufferedUart,
    stack: Stack<'static>,
    port: u16,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        socket.accept(port).await.map_err(|_| ())?;
        if exchange_link_handshake(
            &mut socket,
            false,
            runtime.handshake_magic,
            runtime.handshake_timeout_ms,
        )
            .await
            .is_err()
        {
            socket.abort();
            let _ = socket.flush().await;
            runtime.link_active.store(false, Ordering::Relaxed);
            continue;
        }
        runtime.link_active.store(true, Ordering::Relaxed);

        let _ = session(uart, &mut socket, spi, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

/// Bridges framed SPI transactions to the TCP socket.
async fn session(
    _uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    spi: &mut UpstreamSpiDevice,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
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
        // Yield cooperatively without adding a fixed 1 ms service gap to the SPI hot path.
        yield_now().await;
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
        self.abort_dma();
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
        self.frame_reported = false;
        p.dmacr().write_value({
            let mut w = rp_pac::spi::regs::Dmacr(0);
            w.set_rxdmae(true);
            w.set_txdmae(true);
            w
        });
        self.start_dma();
    }

    /// Samples the hardware chip-select line to detect transaction edges.
    fn cs_is_low(&self) -> bool {
        !rp_pac::IO_BANK0.gpio(13).status().read().infrompad()
    }

    /// Starts full-frame DMA servicing for the current SPI transaction buffers.
    fn start_dma(&mut self) {
        let spi = rp_pac::SPI1;
        let dr = spi.dr().as_ptr();
        let tx_dma_ch = self.tx_dma_ch;
        let rx_dma_ch = self.rx_dma_ch;
        let tx_frame = self.tx_frame.as_ptr();
        let rx_frame = self.rx_frame.as_mut_ptr();

        Self::configure_dma_channel(
            tx_dma_ch,
            tx_frame,
            dr as *mut u8,
            true,
            false,
            TreqSel::SPI1_TX,
        );
        Self::configure_dma_channel(
            rx_dma_ch,
            dr as *const u8,
            rx_frame,
            false,
            true,
            TreqSel::SPI1_RX,
        );
    }

    /// Polls the active SPI transaction and yields a complete frame once fully received.
    fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        let cs_low = self.cs_is_low();

        if cs_low && !self.cs_active {
            self.cs_active = true;
            self.frame_reported = false;
        } else if !cs_low && self.cs_active {
            self.cs_active = false;
            self.finish_transaction();
            return None;
        }

        if !self.cs_active {
            if !self.frame_reported && !self.dma_channel_busy(self.rx_dma_ch) {
                self.frame_reported = true;
                return Some(&self.rx_frame);
            }

            // Short transfers can begin and end entirely between polls, leaving DMA mid-frame
            // with CS already deasserted. Detect that stale state and rearm immediately.
            if self.transaction_started() {
                self.finish_transaction();
            }
            return None;
        }

        if !self.frame_reported && !self.dma_channel_busy(self.rx_dma_ch) {
            self.frame_reported = true;
            return Some(&self.rx_frame);
        }
        None
    }

    /// Programs one DMA channel for a single SPI transaction direction.
    fn configure_dma_channel(
        channel: u8,
        read_addr: *const u8,
        write_addr: *mut u8,
        incr_read: bool,
        incr_write: bool,
        dreq: TreqSel,
    ) {
        let ch = rp_pac::DMA.ch(channel as usize);
        ch.read_addr().write_value(read_addr as u32);
        ch.write_addr().write_value(write_addr as u32);
        ch.trans_count().write(|w| {
            *w = FRAME_SIZE as u32;
        });

        compiler_fence(MemoryOrdering::SeqCst);
        ch.ctrl_trig().write(|w| {
            w.set_treq_sel(dreq);
            w.set_data_size(DataSize::SIZE_BYTE);
            w.set_incr_read(incr_read);
            w.set_incr_write(incr_write);
            w.set_chain_to(channel);
            w.set_en(true);
        });
        compiler_fence(MemoryOrdering::SeqCst);
    }

    /// Returns whether the selected DMA channel is still active.
    fn dma_channel_busy(&self, channel: u8) -> bool {
        rp_pac::DMA.ch(channel as usize).ctrl_trig().read().busy()
    }

    /// Returns whether the current DMA transaction has consumed any bytes.
    fn transaction_started(&self) -> bool {
        self.dma_remaining_count(self.rx_dma_ch) < FRAME_SIZE as u32
            || self.dma_remaining_count(self.tx_dma_ch) < FRAME_SIZE as u32
    }

    /// Returns the DMA transfer count remaining for the selected channel.
    fn dma_remaining_count(&self, channel: u8) -> u32 {
        rp_pac::DMA.ch(channel as usize).trans_count().read()
    }

    /// Finalizes the current CS-bounded transaction and rearms the slave for the next one.
    fn finish_transaction(&mut self) {
        self.abort_dma();
        if self.clear_after_transaction {
            self.clear_after_transaction = false;
            self.tx_frame.fill(0);
            self.tx_frame[0] = RESP_DATA_MAGIC;
            self.tx_frame[1] = 0;
        }
        self.begin_transaction();
    }

    /// Aborts any in-flight SPI DMA transfers and disables SPI DMA requests.
    fn abort_dma(&self) {
        rp_pac::SPI1.dmacr().write_value({
            let mut w = rp_pac::spi::regs::Dmacr(0);
            w.set_rxdmae(false);
            w.set_txdmae(false);
            w
        });
        rp_pac::DMA.chan_abort().modify(|m| {
            m.set_chan_abort((1 << self.tx_dma_ch) | (1 << self.rx_dma_ch));
        });
        while self.dma_channel_busy(self.tx_dma_ch) || self.dma_channel_busy(self.rx_dma_ch) {}
    }
}
