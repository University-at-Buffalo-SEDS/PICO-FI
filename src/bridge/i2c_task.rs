//! Dedicated I2C polling task for the slot-based upstream protocol.

use crate::bridge::commands::{render_local_bridge_command, trim_ascii_line};
use crate::bridge::overwrite_queue::OverwriteQueue;
use crate::config::BridgeConfig;
use embassy_futures::select::{Either, select};
use embassy_rp::i2c_slave::{Command, I2cSlave, ReadStatus};
use embassy_rp::peripherals::I2C0;
use embassy_time::{Duration, Instant, Timer};
use heapless::Vec;
use portable_atomic::AtomicBool;

const SLOT_SIZE: usize = 32;
const LISTEN_BUFFER_SIZE: usize = 64;
const SLOT_HEADER_SIZE: usize = 18;
const SLOT_PAYLOAD_SIZE: usize = SLOT_SIZE - SLOT_HEADER_SIZE;

const SLOT_MAGIC0: u8 = 0x49;
const SLOT_MAGIC1: u8 = 0x32;
const SLOT_VERSION: u8 = 1;

const KIND_IDLE: u8 = 0x00;
const KIND_DATA: u8 = 0x01;
const KIND_COMMAND: u8 = 0x02;
const KIND_ERROR: u8 = 0x7F;

const FLAG_START: u8 = 0x01;
const FLAG_END: u8 = 0x02;

const RESPONSE_WAIT_MS: u64 = 20;
const PARTIAL_PACKET_TIMEOUT: Duration = Duration::from_millis(50);
pub const I2C_PACKET_MAX: usize = 1024;
const INVALID_SLOT_MSG: &[u8] = b"error invalid i2c slot";
const INVALID_KIND_MSG: &[u8] = b"error invalid i2c kind";
const OVERSIZE_MSG: &[u8] = b"error i2c payload too large";

/// Variable-length message passed between the I2C bus task and bridge session.
pub struct I2cPacket {
    pub kind: u8,
    pub payload: Vec<u8, I2C_PACKET_MAX>,
}

#[derive(Clone, Copy)]
struct Slot {
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: usize,
    total_len: usize,
    data_len: usize,
    data: [u8; SLOT_PAYLOAD_SIZE],
}

struct RxPacketState {
    active: bool,
    kind: u8,
    transfer_id: u16,
    total_len: usize,
    next_offset: usize,
    payload: [u8; I2C_PACKET_MAX],
    started_at: Option<Instant>,
}

impl RxPacketState {
    const fn new() -> Self {
        Self {
            active: false,
            kind: KIND_IDLE,
            transfer_id: 0,
            total_len: 0,
            next_offset: 0,
            payload: [0u8; I2C_PACKET_MAX],
            started_at: None,
        }
    }

    fn reset(&mut self) {
        self.active = false;
        self.kind = KIND_IDLE;
        self.transfer_id = 0;
        self.total_len = 0;
        self.next_offset = 0;
        self.payload = [0u8; I2C_PACKET_MAX];
        self.started_at = None;
    }

    fn reset_if_stale(&mut self) {
        if !self.active {
            return;
        }

        if self
            .started_at
            .map(|started| Instant::now().duration_since(started) >= PARTIAL_PACKET_TIMEOUT)
            .unwrap_or(false)
        {
            self.reset();
        }
    }

    fn push_slot(&mut self, slot: Slot) -> Result<Option<(u8, u16, usize)>, &'static [u8]> {
        self.reset_if_stale();

        if slot.total_len > I2C_PACKET_MAX {
            self.reset();
            return Err(OVERSIZE_MSG);
        }

        if (slot.flags & FLAG_START) != 0 {
            self.reset();
            if slot.offset != 0 || slot.data_len > slot.total_len {
                return Err(INVALID_SLOT_MSG);
            }

            self.active = true;
            self.kind = slot.kind;
            self.transfer_id = slot.transfer_id;
            self.total_len = slot.total_len;
            self.next_offset = slot.data_len;
            self.started_at = Some(Instant::now());
            if slot.data_len != 0 {
                self.payload[..slot.data_len].copy_from_slice(&slot.data[..slot.data_len]);
            }
        } else {
            if !self.active {
                return Err(INVALID_SLOT_MSG);
            }
            if slot.kind != self.kind
                || slot.transfer_id != self.transfer_id
                || slot.offset != self.next_offset
                || self.next_offset + slot.data_len > self.total_len
            {
                self.reset();
                return Err(INVALID_SLOT_MSG);
            }

            if slot.data_len != 0 {
                let end = self.next_offset + slot.data_len;
                self.payload[self.next_offset..end].copy_from_slice(&slot.data[..slot.data_len]);
                self.next_offset = end;
            }
        }

        if (slot.flags & FLAG_END) != 0 {
            if !self.active || self.next_offset != self.total_len {
                self.reset();
                return Err(INVALID_SLOT_MSG);
            }

            let result = Some((self.kind, self.transfer_id, self.total_len));
            self.active = false;
            self.kind = KIND_IDLE;
            self.transfer_id = 0;
            self.total_len = 0;
            self.next_offset = 0;
            self.started_at = None;
            Ok(result)
        } else {
            Ok(None)
        }
    }

    fn payload(&self, len: usize) -> &[u8] {
        &self.payload[..len]
    }
}

struct TxPacketState {
    active: bool,
    kind: u8,
    transfer_id: u16,
    payload_len: usize,
    next_offset: usize,
    payload: [u8; I2C_PACKET_MAX],
}

impl TxPacketState {
    const fn new() -> Self {
        Self {
            active: false,
            kind: KIND_IDLE,
            transfer_id: 1,
            payload_len: 0,
            next_offset: 0,
            payload: [0u8; I2C_PACKET_MAX],
        }
    }

    fn reset(&mut self) {
        self.active = false;
        self.kind = KIND_IDLE;
        self.payload_len = 0;
        self.next_offset = 0;
        self.payload = [0u8; I2C_PACKET_MAX];
    }

    fn stage(&mut self, kind: u8, transfer_id: u16, payload: &[u8]) {
        self.reset();
        let len = payload.len().min(I2C_PACKET_MAX);
        self.active = true;
        self.kind = kind;
        self.transfer_id = transfer_id;
        self.payload_len = len;
        self.payload[..len].copy_from_slice(&payload[..len]);
    }

    fn stage_error(&mut self, transfer_id: u16, payload: &[u8]) {
        self.stage(KIND_ERROR, transfer_id, payload);
    }

    fn stage_idle(&mut self, transfer_id: u16) {
        self.transfer_id = transfer_id;
        self.reset();
    }

    fn stage_bridge_packet(&mut self, transfer_id: u16, packet: I2cPacket) {
        match packet.kind {
            KIND_DATA | KIND_COMMAND | KIND_ERROR => {
                self.stage(packet.kind, transfer_id, packet.payload.as_slice())
            }
            _ => self.stage_error(transfer_id, INVALID_SLOT_MSG),
        };
    }

    fn has_pending(&self) -> bool {
        self.active
    }

    fn next_slot_bytes(&mut self) -> [u8; SLOT_SIZE] {
        if !self.active {
            return [0u8; SLOT_SIZE];
        }

        let offset = self.next_offset;
        let remaining = self.payload_len.saturating_sub(offset);
        let data_len = remaining.min(SLOT_PAYLOAD_SIZE);
        let end = offset + data_len;

        let mut data = [0u8; SLOT_PAYLOAD_SIZE];
        if data_len != 0 {
            data[..data_len].copy_from_slice(&self.payload[offset..end]);
        }

        let mut flags = 0u8;
        if offset == 0 {
            flags |= FLAG_START;
        }
        if end >= self.payload_len {
            flags |= FLAG_END;
        }

        let slot = encode_slot(
            self.kind,
            flags,
            self.transfer_id,
            offset,
            self.payload_len,
            data_len,
            &data,
        );

        if end >= self.payload_len {
            self.reset();
        } else {
            self.next_offset = end;
        }

        slot
    }
}

/// Continuously services the I2C bus, translating 32-byte slots into bridge packets.
pub async fn i2c_poll_task(
    i2c: &mut I2cSlave<'static, I2C0>,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: &'static OverwriteQueue<I2cPacket, 8>,
    rx_resp: &'static OverwriteQueue<I2cPacket, 8>,
) -> ! {
    let mut transaction_buf = [0u8; LISTEN_BUFFER_SIZE];
    let mut rx_packet = RxPacketState::new();
    let mut tx_packet = TxPacketState::new();
    let mut last_transfer_id = 1u16;

    loop {
        match i2c.listen(&mut transaction_buf).await {
            Ok(Command::Write(len)) => {
                handle_write_slot(
                    &transaction_buf[..len.min(SLOT_SIZE)],
                    &mut rx_packet,
                    &mut tx_packet,
                    &mut last_transfer_id,
                    bridge_config,
                    link_active,
                    tx,
                );
            }
            Ok(Command::WriteRead(len)) => {
                handle_write_slot(
                    &transaction_buf[..len.min(SLOT_SIZE)],
                    &mut rx_packet,
                    &mut tx_packet,
                    &mut last_transfer_id,
                    bridge_config,
                    link_active,
                    tx,
                );
                await_response_packet(&mut tx_packet, rx_resp, last_transfer_id).await;
                let _ = respond_slot(i2c, &mut tx_packet).await;
            }
            Ok(Command::Read) => {
                await_response_packet(&mut tx_packet, rx_resp, last_transfer_id).await;
                let _ = respond_slot(i2c, &mut tx_packet).await;
            }
            Ok(Command::GeneralCall(_)) => {}
            Err(_) => recover_i2c_state(i2c, &mut rx_packet, &mut tx_packet, last_transfer_id),
        }
    }
}

fn handle_write_slot(
    raw: &[u8],
    rx_packet: &mut RxPacketState,
    tx_packet: &mut TxPacketState,
    last_transfer_id: &mut u16,
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx: &'static OverwriteQueue<I2cPacket, 8>,
) {
    match decode_slot(raw) {
        Ok(None) => {}
        Ok(Some(slot)) => match rx_packet.push_slot(slot) {
            Ok(Some((kind, transfer_id, total_len))) => {
                *last_transfer_id = nonzero_transfer_id(transfer_id);
                process_complete_packet(
                    kind,
                    transfer_id,
                    rx_packet.payload(total_len),
                    bridge_config,
                    link_active,
                    tx_packet,
                    tx,
                );
            }
            Ok(None) => {}
            Err(msg) => tx_packet.stage_error(*last_transfer_id, msg),
        },
        Err(msg) => tx_packet.stage_error(*last_transfer_id, msg),
    }
}

fn process_complete_packet(
    kind: u8,
    transfer_id: u16,
    payload: &[u8],
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    tx_packet: &mut TxPacketState,
    tx: &'static OverwriteQueue<I2cPacket, 8>,
) {
    match kind {
        KIND_COMMAND => {
            if looks_like_local_command(payload) {
                let line = trim_ascii_line(payload);
                let response = render_local_bridge_command(bridge_config, link_active, line);
                tx_packet.stage(KIND_COMMAND, nonzero_transfer_id(transfer_id), response.as_bytes());
            } else if let Ok(packet) = make_data_packet(payload) {
                tx.push_overwrite(packet);
            } else {
                tx_packet.stage_error(nonzero_transfer_id(transfer_id), OVERSIZE_MSG);
            }
        }
        KIND_DATA => {
            if payload.is_empty() {
                return;
            }
            if let Ok(packet) = make_data_packet(payload) {
                tx.push_overwrite(packet);
            } else {
                tx_packet.stage_error(nonzero_transfer_id(transfer_id), OVERSIZE_MSG);
            }
        }
        _ => tx_packet.stage_error(nonzero_transfer_id(transfer_id), INVALID_KIND_MSG),
    }
}

async fn await_response_packet(
    tx_packet: &mut TxPacketState,
    rx_resp: &'static OverwriteQueue<I2cPacket, 8>,
    transfer_id: u16,
) {
    if tx_packet.has_pending() {
        return;
    }

    if let Some(resp) = rx_resp.try_pop_latest() {
        tx_packet.stage_bridge_packet(transfer_id, resp);
        return;
    }

    match select(rx_resp.pop_latest(), Timer::after_millis(RESPONSE_WAIT_MS)).await {
        Either::First(resp) => tx_packet.stage_bridge_packet(transfer_id, resp),
        Either::Second(_) => tx_packet.stage_idle(transfer_id),
    }
}

async fn respond_slot(
    i2c: &mut I2cSlave<'static, I2C0>,
    tx_packet: &mut TxPacketState,
) -> Result<(), ()> {
    let slot = tx_packet.next_slot_bytes();

    match i2c.respond_to_read(&slot).await {
        Ok(ReadStatus::Done) => Ok(()),
        Ok(ReadStatus::LeftoverBytes(_)) | Ok(ReadStatus::NeedMoreBytes) | Err(_) => Err(()),
    }
}

fn recover_i2c_state(
    i2c: &mut I2cSlave<'static, I2C0>,
    rx_packet: &mut RxPacketState,
    tx_packet: &mut TxPacketState,
    transfer_id: u16,
) {
    i2c.reset();
    rx_packet.reset();
    tx_packet.stage_error(transfer_id, INVALID_SLOT_MSG);
}

fn decode_slot(raw: &[u8]) -> Result<Option<Slot>, &'static [u8]> {
    if raw.len() != SLOT_SIZE {
        return Err(INVALID_SLOT_MSG);
    }
    if raw.iter().all(|&byte| byte == 0x00) || raw.iter().all(|&byte| byte == 0xFF) {
        return Ok(None);
    }
    if raw[0] != SLOT_MAGIC0 || raw[1] != SLOT_MAGIC1 || raw[2] != SLOT_VERSION {
        return Err(INVALID_SLOT_MSG);
    }

    let kind = raw[3];
    if kind == KIND_IDLE {
        return Ok(None);
    }

    let data_len = u16::from_le_bytes([raw[14], raw[15]]) as usize;
    if data_len > SLOT_PAYLOAD_SIZE {
        return Err(INVALID_SLOT_MSG);
    }

    let mut data = [0u8; SLOT_PAYLOAD_SIZE];
    if data_len != 0 {
        data[..data_len].copy_from_slice(&raw[SLOT_HEADER_SIZE..SLOT_HEADER_SIZE + data_len]);
    }

    Ok(Some(Slot {
        kind,
        flags: raw[4],
        transfer_id: u16::from_le_bytes([raw[16], raw[17]]),
        offset: u32::from_le_bytes([raw[6], raw[7], raw[8], raw[9]]) as usize,
        total_len: u32::from_le_bytes([raw[10], raw[11], raw[12], raw[13]]) as usize,
        data_len,
        data,
    }))
}

fn encode_slot(
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: usize,
    total_len: usize,
    data_len: usize,
    data: &[u8; SLOT_PAYLOAD_SIZE],
) -> [u8; SLOT_SIZE] {
    let mut raw = [0u8; SLOT_SIZE];
    raw[0] = SLOT_MAGIC0;
    raw[1] = SLOT_MAGIC1;
    raw[2] = SLOT_VERSION;
    raw[3] = kind;
    raw[4] = flags;
    raw[6..10].copy_from_slice(&(offset as u32).to_le_bytes());
    raw[10..14].copy_from_slice(&(total_len as u32).to_le_bytes());
    raw[14..16].copy_from_slice(&(data_len as u16).to_le_bytes());
    raw[16..18].copy_from_slice(&transfer_id.to_le_bytes());
    if data_len != 0 {
        raw[SLOT_HEADER_SIZE..SLOT_HEADER_SIZE + data_len].copy_from_slice(&data[..data_len]);
    }
    raw
}

fn make_data_packet(payload: &[u8]) -> Result<I2cPacket, ()> {
    let mut data = Vec::<u8, I2C_PACKET_MAX>::new();
    data.extend_from_slice(payload).map_err(|_| ())?;
    Ok(I2cPacket {
        kind: KIND_DATA,
        payload: data,
    })
}

fn looks_like_local_command(payload: &[u8]) -> bool {
    payload.first() == Some(&b'/')
        && payload
            .iter()
            .all(|&byte| byte == b'\n' || byte == b'\r' || (32..=126).contains(&byte))
}

fn nonzero_transfer_id(value: u16) -> u16 {
    if value == 0 { 1 } else { value }
}
