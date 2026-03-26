//! I2C upstream bridge implementation and RP2040 I2C1 slave driver.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::i2c_task::I2cFrame;
use crate::bridge::runtime::BridgeRuntime;
use crate::config::BridgeConfig;
use crate::net::{connect_with_timeout, exchange_link_handshake, write_socket};
use crate::protocol::i2c::{
    FRAME_SIZE, RESP_COMMAND_MAGIC, RESP_DATA_MAGIC, RequestFrame, make_response_frame,
    parse_request_frame,
};
use embassy_futures::select::{Either, select};
use embassy_net::Ipv4Address;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_rp::Peri;
use embassy_rp::peripherals::{I2C1, PIN_2, PIN_3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicBool, Ordering};

const I2C_SLAVE_ADDR: u16 = 0x55;

pub struct UpstreamI2cDevice {
    rx_frame: [u8; FRAME_SIZE],
    tx_frame: [u8; FRAME_SIZE],
    rx_pos: usize,
    tx_pos: usize,
    pending_frame: Option<[u8; FRAME_SIZE]>,
    response_pending: bool,
}

impl UpstreamI2cDevice {
    pub fn new() -> Self {
        init_i2c1_slave();

        let dev = UpstreamI2cDevice {
            rx_frame: [0u8; FRAME_SIZE],
            tx_frame: empty_data_frame(),
            rx_pos: 0,
            tx_pos: 0,
            pending_frame: None,
            response_pending: false,
        };
        let mut dev = dev;
        dev.preload_i2c_tx();
        dev
    }

    pub fn stage_response_frame(&mut self, frame: [u8; FRAME_SIZE]) {
        self.tx_frame = frame;
        self.tx_pos = 0;
        self.response_pending = true;
        self.preload_i2c_tx();
    }

    pub fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        if self.pending_frame.is_none() {
            self.handle_i2c_transaction();
        }
        self.pending_frame.as_ref()
    }

    pub fn clear_pending_frame(&mut self) {
        self.pending_frame = None;
        self.rx_frame.fill(0);
        self.rx_pos = 0;
    }

    fn reset_response_frame(&mut self) {
        self.tx_frame = empty_data_frame();
        self.tx_pos = 0;
        self.response_pending = false;
        self.preload_i2c_tx();
    }

    fn preload_i2c_tx(&mut self) {
        unsafe {
            const I2C1_BASE: usize = 0x4004_8000;
            const IC_DATA_CMD: usize = 0x10;
            const IC_STATUS: usize = 0x70;

            let data_cmd_addr = (I2C1_BASE + IC_DATA_CMD) as *mut u32;
            let status_addr = (I2C1_BASE + IC_STATUS) as *const u32;

            while self.tx_pos < FRAME_SIZE {
                let status = core::ptr::read_volatile(status_addr);
                if (status & 0x02) == 0 {
                    break;
                }
                let byte = self.tx_frame[self.tx_pos] as u32;
                core::ptr::write_volatile(data_cmd_addr, byte);
                self.tx_pos += 1;
            }
        }
    }

    fn handle_i2c_transaction(&mut self) {
        unsafe {
            const I2C1_BASE: usize = 0x4004_8000;
            const IC_RAW_INTR_STAT: usize = 0x34;
            const IC_DATA_CMD: usize = 0x10;
            const IC_STATUS: usize = 0x70;
            const IC_CLR_ACTIVITY: usize = 0x5C;
            const IC_CLR_RX_UNDER: usize = 0x44;
            const IC_CLR_RX_OVER: usize = 0x48;
            const IC_CLR_TX_OVER: usize = 0x4C;
            const IC_CLR_RD_REQ: usize = 0x50;
            const IC_CLR_STOP_DET: usize = 0x60;

            let intr_stat_addr = (I2C1_BASE + IC_RAW_INTR_STAT) as *const u32;
            let data_cmd_addr = (I2C1_BASE + IC_DATA_CMD) as *mut u32;
            let status_addr = (I2C1_BASE + IC_STATUS) as *const u32;
            let intr_stat = core::ptr::read_volatile(intr_stat_addr);

            if (intr_stat & 0x20) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_RD_REQ) as *const u32);

                for _ in 0..FRAME_SIZE {
                    let status = core::ptr::read_volatile(status_addr);
                    if (status & 0x02) == 0 {
                        break;
                    }
                    if self.tx_pos >= FRAME_SIZE {
                        break;
                    }
                    let byte = self.tx_frame[self.tx_pos] as u32;
                    core::ptr::write_volatile(data_cmd_addr, byte);
                    self.tx_pos += 1;
                }
            }

            loop {
                let status = core::ptr::read_volatile(status_addr);
                if (status & 0x08) == 0 {
                    break;
                }

                let data_reg = core::ptr::read_volatile(data_cmd_addr);
                let byte = (data_reg & 0xFF) as u8;
                if self.rx_pos < FRAME_SIZE {
                    self.rx_frame[self.rx_pos] = byte;
                    self.rx_pos += 1;
                }
            }

            if (intr_stat & 0x01) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_RX_UNDER) as *const u32);
            }
            if (intr_stat & 0x08) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_RX_OVER) as *const u32);
            }
            if (intr_stat & 0x10) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_TX_OVER) as *const u32);
            }
            if (intr_stat & 0x100) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_ACTIVITY) as *const u32);
            }

            if (intr_stat & 0x200) != 0 {
                let _ = core::ptr::read_volatile((I2C1_BASE + IC_CLR_STOP_DET) as *const u32);

                if self.rx_pos > 0 {
                    self.pending_frame = Some(self.rx_frame);
                    self.rx_pos = 0;
                    self.rx_frame.fill(0);
                    self.tx_pos = 0;
                } else if self.response_pending {
                    self.reset_response_frame();
                }
            }
        }
    }
}

fn empty_data_frame() -> [u8; FRAME_SIZE] {
    make_response_frame(RESP_DATA_MAGIC, b"")
}

fn init_i2c1_slave() {
    unsafe {
        const I2C1_BASE: usize = 0x4004_8000;
        const IO_BANK0_BASE: usize = 0x4001_4000;
        const IC_CON: usize = 0x00;
        const IC_SAR: usize = 0x08;
        const IC_ENABLE: usize = 0x6C;
        const GPIO_CTRL_OFFSET: usize = 0x04;
        const I2C_FUNCSEL: u32 = 3;

        let gpio2_ctrl = (IO_BANK0_BASE + (2 * 8) + GPIO_CTRL_OFFSET) as *mut u32;
        let gpio3_ctrl = (IO_BANK0_BASE + (3 * 8) + GPIO_CTRL_OFFSET) as *mut u32;
        let gpio2_val = core::ptr::read_volatile(gpio2_ctrl);
        let gpio3_val = core::ptr::read_volatile(gpio3_ctrl);
        core::ptr::write_volatile(gpio2_ctrl, (gpio2_val & !0x1F) | I2C_FUNCSEL);
        core::ptr::write_volatile(gpio3_ctrl, (gpio3_val & !0x1F) | I2C_FUNCSEL);

        let ic_enable = (I2C1_BASE + IC_ENABLE) as *mut u32;
        core::ptr::write_volatile(ic_enable, 0);

        let ic_con = (I2C1_BASE + IC_CON) as *mut u32;
        core::ptr::write_volatile(ic_con, 0x21);

        let ic_sar = (I2C1_BASE + IC_SAR) as *mut u32;
        core::ptr::write_volatile(ic_sar, I2C_SLAVE_ADDR as u32);

        core::ptr::write_volatile(ic_enable, 1);
    }
}

pub fn init_upstream_i2c(
    _i2c1: Peri<'static, I2C1>,
    _sda: Peri<'static, PIN_2>,
    _scl: Peri<'static, PIN_3>,
) -> UpstreamI2cDevice {
    UpstreamI2cDevice::new()
}

pub async fn run_client(
    stack: Stack<'static>,
    host: [u8; 4],
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
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

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            i2c_rx,
            i2c_tx,
        )
        .await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
        Timer::after_millis(runtime.reconnect_delay_ms).await;
    }
}

pub async fn run_server(
    stack: Stack<'static>,
    port: u16,
    bridge_config: BridgeConfig,
    runtime: BridgeRuntime<'_>,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
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

        let _ = session(
            &mut socket,
            bridge_config,
            runtime.link_active,
            i2c_rx,
            i2c_tx,
        )
        .await;
        socket.abort();
        let _ = socket.flush().await;
        runtime.link_active.store(false, Ordering::Relaxed);
    }
}

async fn session(
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    i2c_rx: Receiver<'static, CriticalSectionRawMutex, I2cFrame, 4>,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> Result<(), ()> {
    let mut net_buf = [0u8; 256];

    loop {
        match select(i2c_rx.receive(), socket.read(&mut net_buf)).await {
            Either::First(frame) => {
                handle_i2c_request(frame, socket, bridge_config, link_active, i2c_tx).await?;
            }
            Either::Second(Ok(net_n)) => {
                if net_n == 0 {
                    return Ok(());
                }
                let response = make_response_frame(RESP_DATA_MAGIC, &net_buf[..net_n]);
                i2c_tx.send(I2cFrame { data: response }).await;
            }
            Either::Second(Err(_)) => return Err(()),
        }
    }
}

async fn handle_i2c_request(
    frame: I2cFrame,
    socket: &mut TcpSocket<'_>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    i2c_tx: Sender<'static, CriticalSectionRawMutex, I2cFrame, 4>,
) -> Result<(), ()> {
    match parse_request_frame(&frame.data) {
        Some(RequestFrame::Data(payload)) => {
            if !payload.is_empty() {
                write_socket(socket, payload).await?;
            } else {
                let response = make_response_frame(RESP_DATA_MAGIC, b"");
                i2c_tx.send(I2cFrame { data: response }).await;
            }
            Ok(())
        }
        Some(RequestFrame::Command(payload)) => {
            let line = trim_ascii_line(payload);
            let response =
                render_local_bridge_command(bridge_config, link_active, line);
            let frame = make_response_frame(RESP_COMMAND_MAGIC, response.as_bytes());
            i2c_tx.send(I2cFrame { data: frame }).await;
            Ok(())
        }
        None => {
            let response = make_response_frame(RESP_COMMAND_MAGIC, b"error invalid i2c frame");
            i2c_tx.send(I2cFrame { data: response }).await;
            Ok(())
        }
    }
}
