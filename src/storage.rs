//! Flash-backed persistence for the bridge configuration shell.

use crate::config::BridgeConfig;
use crate::config_record::{RECORD_SIZE, decode_record, encode_record};
use embassy_rp::Peri;
use embassy_rp::flash::Blocking;
use embassy_rp::flash::{ERASE_SIZE, Flash};
use embassy_rp::peripherals::FLASH;

/// Total onboard flash size configured by the linker script.
const FLASH_SIZE_BYTES: usize = 2 * 1024 * 1024;

/// Offset of the reserved config sector from the beginning of flash.
const CONFIG_SECTOR_OFFSET: u32 = (FLASH_SIZE_BYTES - ERASE_SIZE) as u32;

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

    /// Stable locally administered MAC derived from this physical device.
    pub fn hardware_mac(&mut self) -> Result<[u8; 6], ()> {
        let mut uid = [0; 8];
        self.flash.blocking_unique_id(&mut uid).map_err(|_| ())?;
        Ok(crate::protocol::discovery::hardware_mac(&uid))
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
