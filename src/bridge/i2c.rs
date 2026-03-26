//! I2C0 slave on GPIO0 (SDA) and GPIO1 (SCL)
//! Uses rp2040-hal to configure hardware I2C peripheral in slave mode

use embassy_time::Timer;
use crate::protocol::spi::FRAME_SIZE;

const I2C_SLAVE_ADDR: u16 = 0x55;

#[derive(Clone, Copy)]
pub struct I2cFrame {
    pub data: [u8; FRAME_SIZE],
}

pub struct UpstreamI2cDevice {
    rx_frame: [u8; FRAME_SIZE],
    tx_frame: [u8; FRAME_SIZE],
    rx_pos: usize,
    tx_pos: usize,
    pending_frame: Option<[u8; FRAME_SIZE]>,
}

impl UpstreamI2cDevice {
    pub fn new() -> Self {
        init_i2c0_slave();

        let mut dev = UpstreamI2cDevice {
            rx_frame: [0u8; FRAME_SIZE],
            tx_frame: [0u8; FRAME_SIZE],
            rx_pos: 0,
            tx_pos: 0,
            pending_frame: None,
        };

        // Initialize with default response (zeros)
        dev.preload_i2c_tx();
        dev
    }

    pub fn stage_response_frame(&mut self, frame: [u8; FRAME_SIZE]) {
        self.tx_frame = frame;
        self.tx_pos = 0;
        // Immediately preload the new data
        self.preload_i2c_tx();
    }

    pub fn poll_transaction(&mut self) -> Option<&[u8; FRAME_SIZE]> {
        if self.pending_frame.is_some() {
            return self.pending_frame.as_ref();
        }

        // Poll I2C status and handle transactions
        self.handle_i2c_transaction();

        self.pending_frame.as_ref()
    }

    pub fn clear_pending_frame(&mut self) {
        self.pending_frame = None;
        self.rx_frame.fill(0);
        self.rx_pos = 0;
    }

    pub async fn receive_frame(&mut self) -> Option<[u8; FRAME_SIZE]> {
        Timer::after_millis(10).await;
        None
    }

    /// Preload TX FIFO with all response data
    fn preload_i2c_tx(&self) {
        unsafe {
            const I2C0_BASE: usize = 0x40044000;
            const IC_DATA_CMD: usize = 0x10;
            const IC_STATUS: usize = 0x70;

            let data_cmd_addr = (I2C0_BASE + IC_DATA_CMD) as *mut u32;
            let status_addr = (I2C0_BASE + IC_STATUS) as *const u32;

            // Load entire frame into TX FIFO (FIFO is 8 bytes, will be drained and refilled)
            for i in 0..FRAME_SIZE {
                // Check TFNF (transmit FIFO not full) bit 1
                let status = core::ptr::read_volatile(status_addr);
                if (status & 0x02) == 0 {
                    break; // FIFO full, stop
                }

                let byte = self.tx_frame[i] as u32;
                core::ptr::write_volatile(data_cmd_addr, byte);
            }
        }
    }

    /// Poll I2C hardware and handle incoming/outgoing data
    fn handle_i2c_transaction(&mut self) {
        unsafe {
            const I2C0_BASE: usize = 0x40044000;
            const IC_RAW_INTR_STAT: usize = 0x34;
            const IC_DATA_CMD: usize = 0x10;
            const IC_STATUS: usize = 0x70;
            const IC_CLR_RX_UNDER: usize = 0x48;
            const IC_CLR_RX_OVER: usize = 0x4C;
            const IC_CLR_TX_OVER: usize = 0x50;
            const IC_CLR_ACTIVITY: usize = 0x5C;
            const IC_CLR_STOP_DET: usize = 0x60;

            let intr_stat_addr = (I2C0_BASE + IC_RAW_INTR_STAT) as *const u32;
            let data_cmd_addr = (I2C0_BASE + IC_DATA_CMD) as *mut u32;
            let status_addr = (I2C0_BASE + IC_STATUS) as *const u32;

            let intr_stat = core::ptr::read_volatile(intr_stat_addr);

            // ALWAYS top up TX FIFO first - this is critical!
            // RD_REQ = bit 5 - master is reading from us
            if (intr_stat & 0x20) != 0 || true {
                // Keep TX FIFO topped up
                let status = core::ptr::read_volatile(status_addr);

                // TFNF (transmit FIFO not full) = bit 1
                for _ in 0..16 {
                    let status = core::ptr::read_volatile(status_addr);
                    if (status & 0x02) == 0 {
                        break; // FIFO full
                    }

                    // Write next byte from current frame
                    if self.tx_pos < FRAME_SIZE {
                        let byte = self.tx_frame[self.tx_pos] as u32;
                        core::ptr::write_volatile(data_cmd_addr, byte);
                        self.tx_pos += 1;
                    } else {
                        // Cycle back to start if we've sent entire frame
                        self.tx_pos = 0;
                        if self.tx_frame[0] != 0 || self.tx_frame[1] != 0 {
                            // Only cycle if not all zeros (default response)
                            break;
                        }
                    }
                }
            }

            // Rx_full = bit 2
            if (intr_stat & 0x04) != 0 {
                let data_reg = core::ptr::read_volatile(data_cmd_addr);
                let byte = (data_reg & 0xFF) as u8;

                if self.rx_pos < FRAME_SIZE {
                    self.rx_frame[self.rx_pos] = byte;
                    self.rx_pos += 1;
                }
            }

            // Clear error interrupts
            if (intr_stat & 0x01) != 0 {
                let _ = core::ptr::read_volatile((I2C0_BASE + IC_CLR_ACTIVITY) as *const u32);
            }
            if (intr_stat & 0x08) != 0 {
                let _ = core::ptr::read_volatile((I2C0_BASE + IC_CLR_RX_UNDER) as *const u32);
            }
            if (intr_stat & 0x20) != 0 {
                let _ = core::ptr::read_volatile((I2C0_BASE + IC_CLR_RX_OVER) as *const u32);
            }
            if (intr_stat & 0x40) != 0 {
                let _ = core::ptr::read_volatile((I2C0_BASE + IC_CLR_TX_OVER) as *const u32);
            }

            // Stop_det = bit 9
            if (intr_stat & 0x200) != 0 {
                let _ = core::ptr::read_volatile((I2C0_BASE + IC_CLR_STOP_DET) as *const u32);

                // Transaction complete
                if self.rx_pos > 0 {
                    self.pending_frame = Some(self.rx_frame);
                    self.rx_pos = 0;
                    self.rx_frame.fill(0);
                    self.tx_pos = 0;
                    // Preload next response
                    self.preload_i2c_tx();
                }
            }
        }
    }
}

/// Initialize I2C0 as slave using volatile pointer access
fn init_i2c0_slave() {
    unsafe {
        // I2C0 base address
        const I2C0_BASE: usize = 0x40044000;

        // IO_BANK0 base address
        const IO_BANK0_BASE: usize = 0x40014000;

        // Register offsets
        const IC_CON: usize = 0x00;
        const IC_SAR: usize = 0x08;
        const IC_ENABLE: usize = 0x6C;
        const GPIO_CTRL_OFFSET: usize = 0x04;

        // Configure GPIO0 for I2C (function 3)
        let gpio0_ctrl_addr = IO_BANK0_BASE + (0 * 8) + GPIO_CTRL_OFFSET;
        let gpio0_ctrl = gpio0_ctrl_addr as *mut u32;
        let val = core::ptr::read_volatile(gpio0_ctrl);
        core::ptr::write_volatile(gpio0_ctrl, (val & !0x1F) | 3);

        // Configure GPIO1 for I2C (function 3)
        let gpio1_ctrl_addr = IO_BANK0_BASE + (1 * 8) + GPIO_CTRL_OFFSET;
        let gpio1_ctrl = gpio1_ctrl_addr as *mut u32;
        let val = core::ptr::read_volatile(gpio1_ctrl);
        core::ptr::write_volatile(gpio1_ctrl, (val & !0x1F) | 3);

        // Disable I2C before configuration
        let ic_enable = (I2C0_BASE + IC_ENABLE) as *mut u32;
        core::ptr::write_volatile(ic_enable, 0);

        // Configure I2C control register
        let ic_con = (I2C0_BASE + IC_CON) as *mut u32;
        // Clear master mode, set slave mode, set speed to 1 (standard 100kHz)
        core::ptr::write_volatile(ic_con, 0x21); // slave_disable=0, master_mode=0, speed=1

        // Set slave address to 0x55
        let ic_sar = (I2C0_BASE + IC_SAR) as *mut u32;
        core::ptr::write_volatile(ic_sar, 0x55);

        // Enable I2C
        core::ptr::write_volatile(ic_enable, 1);
    }
}

pub fn init_upstream_spi<TX, RX>(
    _spi1: embassy_rp::Peri<'static, embassy_rp::peripherals::SPI1>,
    _sclk: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_10>,
    _tx: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_11>,
    _rx: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_12>,
    _cs: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_13>,
    _tx_dma: embassy_rp::Peri<'static, TX>,
    _rx_dma: embassy_rp::Peri<'static, RX>,
) -> UpstreamI2cDevice
where
    TX: embassy_rp::PeripheralType,
    RX: embassy_rp::PeripheralType,
{
    UpstreamI2cDevice::new()
}


