//! SPI upstream bridge implementation and SPI slave framing state.

use crate::bridge::commands::{trim_ascii_line};
use crate::bridge::runtime::BridgeRuntime;
use crate::bridge::spi_task::SpiFrame;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::spi::{
    FRAME_SIZE, PAYLOAD_MAX, RESP_DATA_MAGIC, RESP_COMMAND_MAGIC, RequestFrame, parse_request_frame,
    make_response_frame,
};
use embassy_futures::yield_now;
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::Peri;
use embassy_rp::peripherals::{PIN_10, PIN_11, PIN_12, PIN_13, SPI1};
use embassy_rp::uart::BufferedUart;
use embassy_rp::PeripheralType;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use embedded_hal::spi::MODE_0;
use embedded_hal_nb::spi::FullDuplex;
use portable_atomic::{AtomicBool, Ordering};
use rp2040_hal::gpio::{self, FunctionSpi, PullDown, bank0};
use rp2040_hal::pac as hal_pac;
use rp2040_hal::spi::{Enabled as HalSpiEnabled, Spi as HalSpi};

type SpiSckPin = gpio::Pin<bank0::Gpio10, FunctionSpi, PullDown>;
type SpiTxPin = gpio::Pin<bank0::Gpio11, FunctionSpi, PullDown>;
type SpiRxPin = gpio::Pin<bank0::Gpio12, FunctionSpi, PullDown>;
type SpiCsPin = gpio::Pin<bank0::Gpio13, FunctionSpi, PullDown>;
type SpiPinout = (SpiTxPin, SpiRxPin, SpiSckPin);
type SlaveSpi = HalSpi<HalSpiEnabled, hal_pac::SPI1, SpiPinout, 8>;

/// Stateful SPI slave transport that buffers one request and one response frame.
pub struct UpstreamSpiDevice {
    /// HAL-managed SPI1 peripheral configured in slave mode.
    spi: SlaveSpi,
    /// Owns the CS pin configuration for the lifetime of the slave transport.
    _cs: SpiCsPin,
    /// Contains the next response frame to transmit back to the SPI master.
    tx_frame: [u8; FRAME_SIZE],
    /// Captures the most recent request frame received from the SPI master.
    rx_frame: [u8; FRAME_SIZE],
    /// Number of response bytes already queued into the hardware TX FIFO.
    tx_len: usize,
    /// Number of request bytes captured for the current transaction.
    rx_len: usize,
    /// Indicates whether chip-select is currently asserted.
    cs_active: bool,
    /// Clears a one-shot response payload after the master finishes reading it.
    clear_after_transaction: bool,
    /// Holds one completed request frame until the bridge loop consumes it.
    pending_frame: Option<[u8; FRAME_SIZE]>,
    /// Tracks offset into tx_frame for chunked response transmission.
    tx_response_offset: usize,
}

/// Configures `SPI1` as the framed upstream slave transport.
pub fn init_upstream_spi<TX: PeripheralType, RX: PeripheralType>(
    spi1: Peri<'static, SPI1>,
    sclk: Peri<'static, PIN_10>,
    tx: Peri<'static, PIN_11>,
    rx: Peri<'static, PIN_12>,
    cs: Peri<'static, PIN_13>,
    _tx_dma: Peri<'static, TX>,
    _rx_dma: Peri<'static, RX>,
) -> UpstreamSpiDevice {
    let _ = (spi1, sclk, tx, rx, cs);
    let mut pac = unsafe { hal_pac::Peripherals::steal() };

    let sck_pin = match unsafe {
        gpio::new_pin(gpio::DynPinId {
            bank: gpio::DynBankId::Bank0,
            num: 10,
        })
    }
    .try_into_pin::<bank0::Gpio10>()
    {
        Ok(pin) => pin.into_pull_type::<PullDown>().into_function::<FunctionSpi>(),
        Err(_) => panic!("GPIO10 must be valid for SPI1 SCK"),
    };
    let tx_pin = match unsafe {
        gpio::new_pin(gpio::DynPinId {
            bank: gpio::DynBankId::Bank0,
            num: 11,
        })
    }
    .try_into_pin::<bank0::Gpio11>()
    {
        Ok(pin) => pin.into_pull_type::<PullDown>().into_function::<FunctionSpi>(),
        Err(_) => panic!("GPIO11 must be valid for SPI1 TX"),
    };
    let rx_pin = match unsafe {
        gpio::new_pin(gpio::DynPinId {
            bank: gpio::DynBankId::Bank0,
            num: 12,
        })
    }
    .try_into_pin::<bank0::Gpio12>()
    {
        Ok(pin) => pin.into_pull_type::<PullDown>().into_function::<FunctionSpi>(),
        Err(_) => panic!("GPIO12 must be valid for SPI1 RX"),
    };
    let cs_pin = match unsafe {
        gpio::new_pin(gpio::DynPinId {
            bank: gpio::DynBankId::Bank0,
            num: 13,
        })
    }
    .try_into_pin::<bank0::Gpio13>()
    {
        Ok(pin) => pin.into_pull_type::<PullDown>().into_function::<FunctionSpi>(),
        Err(_) => panic!("GPIO13 must be valid for SPI1 CS"),
    };

    let spi = HalSpi::<_, _, _, 8>::new(pac.SPI1, (tx_pin, rx_pin, sck_pin)).init_slave(&mut pac.RESETS, MODE_0);

    let mut device = UpstreamSpiDevice {
        spi,
        _cs: cs_pin,
        tx_frame: [0; FRAME_SIZE],
        rx_frame: [0; FRAME_SIZE],
        tx_len: 0,
        rx_len: 0,
        cs_active: false,
        clear_after_transaction: false,
        pending_frame: None,
        tx_response_offset: 0,
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
    _spi: Option<&mut UpstreamSpiDevice>,
    spi_rx: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    spi_tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
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

        // Try to connect while also processing SPI commands
        loop {
            // Process any pending SPI commands while waiting for connection
            process_spi_commands(&spi_rx, &spi_tx, bridge_config, runtime.link_active).await;

            if connect_with_timeout(&mut socket, remote, port, runtime.connect_timeout_ms).await.is_ok() {
                break;
            }
            Timer::after_millis(runtime.reconnect_delay_ms).await;
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

        let _ = session(uart, &mut socket, spi_rx, spi_tx, bridge_config, runtime.link_active).await;
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
    _spi: Option<&mut UpstreamSpiDevice>,
    spi_rx: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    spi_tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
) -> Result<(), ()> {
    loop {
        let mut rx_buf = [0u8; 2048];
        let mut tx_buf = [0u8; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_keep_alive(Some(Duration::from_secs(5)));

        // Wait for TCP connection while still processing SPI commands
        loop {
            // Process any pending SPI commands (e.g., /ping) even before TCP connects
            process_spi_commands(&spi_rx, &spi_tx, bridge_config, runtime.link_active).await;

            // Try to accept with a short timeout so we don't starve SPI processing
            if socket.accept(port).await.is_ok() {
                break;
            }

            yield_now().await;
        }

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

        let _ = session(uart, &mut socket, spi_rx, spi_tx, bridge_config, runtime.link_active).await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

/// Bridges framed SPI transactions to the TCP socket.
/// Receives SPI frames from core 1 via channel instead of polling directly.
/// Sends response frames back to core 1 via a separate channel.
async fn session(
    _uart: &mut BufferedUart,
    socket: &mut TcpSocket<'_>,
    spi_rx: Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    spi_tx: Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    _bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) -> Result<(), ()> {
    let mut net_buf = [0u8; PAYLOAD_MAX];

    loop {
        if !socket.may_recv() && !socket.can_recv() {
            return Ok(());
        }
        if socket.recv_queue() > 0 {
            let net_n = socket.read(&mut net_buf).await.map_err(|_| ())?;
            if net_n == 0 {
                return Ok(());
            }
            // Send network data back through SPI as a data response
            let resp = SpiFrame {
                data: make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]),
            };
            let _ = spi_tx.try_send(resp);
        }

        // Receive frames from core 1 via channel instead of polling
        if let Ok(frame) = spi_rx.try_receive() {
            match parse_request_frame(&frame.data) {
                Some(RequestFrame::Data(payload)) if !payload.is_empty() => {
                    write_socket(socket, payload).await?;
                }
                Some(RequestFrame::Command(payload)) => {
                    let command = trim_ascii_line(payload);
                    // Use direct byte array response builder (same as process_spi_commands)
                    let resp = build_command_response_frame(command, link_active);
                    let _ = spi_tx.try_send(resp);
                }
                _ => {}
            }
        }
        yield_now().await;
    }
}

/// Helper to build response frames directly with byte arrays, avoiding heapless::String
fn build_command_response_frame(command: &str, _link_active: &AtomicBool) -> SpiFrame {
    let mut data = [0u8; FRAME_SIZE];

    // Diagnostic: show what we received
    let cmd_bytes = command.as_bytes();

    let response_text: &[u8] = if cmd_bytes.len() == 0 {
        b"empty"
    } else if cmd_bytes[0] == 0x2F {  // 0x2F = '/'
        // Got a slash - show next byte
        if cmd_bytes.len() > 1 {
            match cmd_bytes[1] {
                0x70 => b"got-p",   // 'p'
                0x6C => b"got-l",   // 'l'
                0x68 => b"got-h",   // 'h'
                0x73 => b"got-s",   // 's'
                _ => b"got-other"
            }
        } else {
            b"slash-only"
        }
    } else {
        // First byte is not a slash - show what it is
        match cmd_bytes[0] {
            0x70 => b"no-slash-p",
            0x6C => b"no-slash-l",
            0x2f => b"slash",
            _ => b"mystery"
        }
    };

    // Set magic byte and length
    data[0] = RESP_COMMAND_MAGIC;
    let len = response_text.len();
    data[1] = len as u8;

    // Explicitly copy each byte to ensure it's written
    for idx in 0..len {
        data[idx + 2] = response_text[idx];
    }

    // Ensure the frame is returned with all bytes set
    SpiFrame { data }
}

/// Helper function that processes SPI command requests and sends responses back.
/// This runs independently of the TCP session so local commands are always handled.
async fn process_spi_commands(
    spi_rx: &Receiver<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    spi_tx: &Sender<'static, CriticalSectionRawMutex, SpiFrame, 4>,
    _bridge_config: BridgeConfig,
    link_active: &AtomicBool,
) {
    // Check if there's a command frame waiting
    if let Ok(frame) = spi_rx.try_receive() {
        match parse_request_frame(&frame.data) {
            Some(RequestFrame::Command(payload)) => {
                let command = trim_ascii_line(payload);
                // Build response directly without going through heapless::String
                let resp = build_command_response_frame(command, link_active);
                let _ = spi_tx.try_send(resp);
            }
            Some(RequestFrame::Data(payload)) if !payload.is_empty() => {
                // Data frames are only processed when connected, so discard them
            }
            _ => {}
        }
    }
}

impl UpstreamSpiDevice {
    /// Stages a complete response frame for the next SPI transaction.
    /// This is called by the SPI polling task when core 0 sends a response.
    pub fn stage_response_frame(&mut self, frame: [u8; FRAME_SIZE]) {
        // Validate frame has valid magic byte
        let magic = frame[0];
        if magic == 0x5A || magic == 0x5B {
            // Valid response frame
            self.tx_frame = frame;
            self.tx_response_offset = 0;
        }
        // Otherwise keep current response (don't corrupt with garbage)
    }

    /// Stages a response frame for the next SPI transaction.
    fn prepare_response_frame(&mut self, magic: u8, payload: &[u8]) {
        // Use the public make_response_frame to ensure consistency
        let frame = make_response_frame(magic, payload);
        self.tx_frame = frame;
        self.tx_response_offset = 0;
    }

    /// Polls one CS-bounded transaction and yields a complete frame once captured.
    /// Waits for CS to go LOW (transaction start), collects all bytes while LOW,
    /// then waits for CS to go HIGH before returning the complete frame.
    pub fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        if self.pending_frame.is_some() {
            return self.pending_frame.as_ref();
        }

        // Wait for CS to assert (go LOW)
        if !self.cs_is_low() {
            return None;
        }

        // CS is LOW - transaction starting
        self.cs_active = true;

        // Continuously drain RX and refill TX while CS is held LOW
        loop {
            // Drain any received bytes into our frame
            self.drain_rx_fifo();

            // Keep TX FIFO filled to prevent 0xFF garbage
            self.prefill_tx_fifo();

            // Check if CS is still low
            if !self.cs_is_low() {
                break;
            }
        }

        // CS went HIGH - transaction is complete
        self.cs_active = false;

        // Final drain to catch any last bytes
        self.drain_rx_fifo();

        // Reset TX offset for next transaction
        self.tx_response_offset = 0;

        // Return frame if we collected bytes
        if self.rx_len > 0 {
            self.pending_frame = Some(self.rx_frame);
        }

        self.pending_frame.as_ref()
    }

    /// Marks the currently pending frame as consumed by the bridge loop.
    pub fn clear_pending_frame(&mut self) {
        self.pending_frame = None;
        // Clear frame data for next transaction
        self.rx_frame.fill(0);
        self.rx_len = 0;
    }

    /// Preloads bytes into the hardware TX FIFO for chunked response transmission.
    /// Sends bytes until FIFO is full (max 8 bytes in hardware FIFO).
    /// CRITICAL: Only loads valid bytes from tx_frame based on tx_response_offset
    fn prefill_tx_fifo(&mut self) {
        let p = rp_pac::SPI1;

        // Check FIFO can accept more
        loop {
            let sr = p.sr().read();
            if !sr.tnf() {
                // FIFO is full
                break;
            }

            // Make sure offset is valid
            if self.tx_response_offset >= FRAME_SIZE {
                // Cycle back to start (shouldn't happen for normal single-frame responses)
                self.tx_response_offset = 0;
            }

            // Get byte from frame at current offset
            let byte = self.tx_frame[self.tx_response_offset];

            // Write to SPI data register
            p.dr().write_value(rp_pac::spi::regs::Dr(byte as u32));

            // Advance offset
            self.tx_response_offset += 1;
            self.tx_len += 1;
        }
    }

    /// Drains any bytes currently waiting in the hardware RX FIFO.
    fn drain_rx_fifo(&mut self) {
        loop {
            match self.spi.read() {
                Ok(byte) => {
                    if self.rx_len < FRAME_SIZE {
                        self.rx_frame[self.rx_len] = byte;
                        self.rx_len += 1;
                    }
                }
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(_)) => break,
            }
        }
    }

    /// Returns whether the hardware CS pin is currently asserted.
    fn cs_is_low(&self) -> bool {
        !rp_pac::IO_BANK0.gpio(13).status().read().infrompad()
    }

    /// Resets the SPI FIFOs without reimplementing the rest of the SPI configuration.
    fn reset_hw_fifos(&self) {
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
}
