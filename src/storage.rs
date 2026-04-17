//! Flash-backed persistence for the bridge configuration shell.

use crate::config::{AddressMode, BridgeConfig, BridgeMode, Ipv4Config, UartPort, UpstreamMode};
use embassy_rp::flash::Blocking;
use embassy_rp::flash::{Flash, ERASE_SIZE};
use embassy_rp::peripherals::FLASH;
use embassy_rp::Peri;

/// Total onboard flash size configured by the linker script.
const FLASH_SIZE_BYTES: usize = 2 * 1024 * 1024;

/// Offset of the reserved config sector from the beginning of flash.
const CONFIG_SECTOR_OFFSET: u32 = (FLASH_SIZE_BYTES - ERASE_SIZE) as u32;

/// Magic value identifying a valid persisted config record.
const CONFIG_MAGIC: [u8; 4] = *b"PCFG";

/// Persistence format version for forward compatibility.
const CONFIG_VERSION: u8 = 2;

/// Size of the serialized config record stored in flash.
const RECORD_SIZE: usize = 64;

/// Flash-backed store for the persisted bridge config override.
pub struct ConfigStorage {
    /// Blocking RP2040 flash driver used for config reads and writes.
    flash: Flash<'static, FLASH, Blocking, FLASH_SIZE_BYTES>,
}

impl ConfigStorage {
    /// Creates a new config store wrapper over the RP2040 flash peripheral.
    pub fn new(flash: Peri<'static, FLASH>) -> Self {
        Self {
            flash: Flash::new_blocking(flash),
        }
    }

    /// Loads a previously persisted config override if one exists and validates.
    pub fn load(&mut self) -> Option<BridgeConfig> {
        let mut buf = [0u8; RECORD_SIZE];
        self.flash
            .blocking_read(CONFIG_SECTOR_OFFSET, &mut buf)
            .ok()?;
        decode_record(&buf)
    }

    /// Persists the provided config by rewriting the reserved flash sector.
    pub fn save(&mut self, config: BridgeConfig) -> Result<(), ()> {
        let buf = encode_record(config);
        self.flash
            .blocking_erase(
                CONFIG_SECTOR_OFFSET,
                CONFIG_SECTOR_OFFSET + ERASE_SIZE as u32,
            )
            .map_err(|_| ())?;
        self.flash
            .blocking_write(CONFIG_SECTOR_OFFSET, &buf)
            .map_err(|_| ())
    }

    /// Clears any persisted override so the compiled defaults apply again.
    pub fn reset(&mut self) -> Result<(), ()> {
        self.flash
            .blocking_erase(
                CONFIG_SECTOR_OFFSET,
                CONFIG_SECTOR_OFFSET + ERASE_SIZE as u32,
            )
            .map_err(|_| ())
    }
}

/// Serializes a bridge config into the fixed flash record format.
fn encode_record(config: BridgeConfig) -> [u8; RECORD_SIZE] {
    let mut buf = [0xffu8; RECORD_SIZE];
    buf[0..4].copy_from_slice(&CONFIG_MAGIC);
    buf[4] = CONFIG_VERSION;
    buf[5..11].copy_from_slice(&config.mac_address);
    buf[11] = encode_address_mode(config.address_mode);
    match config.address_mode {
        AddressMode::Dhcp => {}
        AddressMode::Static(ipv4) => {
            buf[12..16].copy_from_slice(&ipv4.address);
            buf[16] = ipv4.prefix_len;
            buf[17..21].copy_from_slice(&ipv4.gateway);
            buf[21..25].copy_from_slice(&ipv4.dns);
        }
    }

    buf[25] = encode_bridge_mode(config.bridge_mode);
    match config.bridge_mode {
        BridgeMode::TcpClient { host, port } => {
            buf[26..30].copy_from_slice(&host);
            buf[30..32].copy_from_slice(&port.to_le_bytes());
        }
        BridgeMode::TcpServer { port } => {
            buf[30..32].copy_from_slice(&port.to_le_bytes());
        }
    }

    buf[32] = encode_upstream_mode(config.upstream_mode);
    buf[33] = encode_uart_port(config.uart_port);
    let checksum = checksum32(&buf[..60]);
    buf[60..64].copy_from_slice(&checksum.to_le_bytes());
    buf
}

/// Decodes and validates a persisted flash record into a bridge config.
fn decode_record(buf: &[u8; RECORD_SIZE]) -> Option<BridgeConfig> {
    if buf[0..4] != CONFIG_MAGIC || buf[4] != CONFIG_VERSION {
        return None;
    }

    let expected = u32::from_le_bytes(buf[60..64].try_into().ok()?);
    if checksum32(&buf[..60]) != expected {
        return None;
    }

    let address_mode = match buf[11] {
        0 => AddressMode::Dhcp,
        1 => AddressMode::Static(Ipv4Config {
            address: buf[12..16].try_into().ok()?,
            prefix_len: buf[16],
            gateway: buf[17..21].try_into().ok()?,
            dns: buf[21..25].try_into().ok()?,
        }),
        _ => return None,
    };

    let bridge_mode = match buf[25] {
        0 => BridgeMode::TcpClient {
            host: buf[26..30].try_into().ok()?,
            port: u16::from_le_bytes(buf[30..32].try_into().ok()?),
        },
        1 => BridgeMode::TcpServer {
            port: u16::from_le_bytes(buf[30..32].try_into().ok()?),
        },
        _ => return None,
    };

    let upstream_mode = match buf[32] {
        0 => UpstreamMode::Uart,
        1 => UpstreamMode::I2c,
        2 => UpstreamMode::Usb,
        3 => UpstreamMode::Test,
        4 => UpstreamMode::Spi,
        5 => UpstreamMode::SpiEcho,
        6 => UpstreamMode::SpiStatic,
        7 => UpstreamMode::SpiLineHigh,
        _ => return None,
    };

    let uart_port = match buf[33] {
        0 => UartPort::Uart0,
        1 => UartPort::Uart1,
        _ => return None,
    };

    Some(BridgeConfig {
        mac_address: buf[5..11].try_into().ok()?,
        address_mode,
        bridge_mode,
        upstream_mode,
        uart_port,
    })
}

/// Encodes the address mode into the compact flash record representation.
fn encode_address_mode(mode: AddressMode) -> u8 {
    match mode {
        AddressMode::Dhcp => 0,
        AddressMode::Static(_) => 1,
    }
}

/// Encodes the bridge mode into the compact flash record representation.
fn encode_bridge_mode(mode: BridgeMode) -> u8 {
    match mode {
        BridgeMode::TcpClient { .. } => 0,
        BridgeMode::TcpServer { .. } => 1,
    }
}

/// Encodes the upstream mode into the compact flash record representation.
fn encode_upstream_mode(mode: UpstreamMode) -> u8 {
    match mode {
        UpstreamMode::Uart => 0,
        UpstreamMode::I2c => 1,
        UpstreamMode::Usb => 2,
        UpstreamMode::Test => 3,
        UpstreamMode::Spi => 4,
        UpstreamMode::SpiEcho => 5,
        UpstreamMode::SpiStatic => 6,
        UpstreamMode::SpiLineHigh => 7,
    }
}

/// Encodes the selected UART into the compact flash record representation.
fn encode_uart_port(port: UartPort) -> u8 {
    match port {
        UartPort::Uart0 => 0,
        UartPort::Uart1 => 1,
    }
}

/// Computes a simple checksum for corruption detection on the flash record.
fn checksum32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for &byte in bytes {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
