//! Linux host-side I2C backend for Pico-Fi framed transport.
//!
//! This module is intended for std-based groundstation applications, not the
//! embedded firmware crate. Copy it into the host application or include it via
//! `#[path = "..."] mod i2c_backend;`.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::os::unix::fs::OpenOptionsExt;
use std::thread;
use std::time::Duration;

const I2C_M_RD: u16 = 0x0001;
const I2C_RDWR: c_ulong = 0x0707;
const FRAME_HEADER_SIZE: usize = 4;
const PAYLOAD_MAX: usize = 4092;
const SLOT_SIZE: usize = 32;
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

const REQ_DATA_MAGIC: u8 = 0xA5;
const REQ_COMMAND_MAGIC: u8 = 0xA6;
const RESP_DATA_MAGIC: u8 = 0x5A;
const RESP_COMMAND_MAGIC: u8 = 0x5B;

#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

#[repr(C)]
struct I2cRdwrIoctlData {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(Vec<u8>),
    Command(Vec<u8>),
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidFrame { magic: u8, len: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::InvalidFrame { magic, len } => {
                write!(f, "invalid I2C frame: magic=0x{magic:02x} len={len}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct I2cBackend {
    file: std::fs::File,
    addr: u16,
    chunk_delay: Duration,
    initial_wait: Duration,
    next_transfer_id: u16,
}

impl I2cBackend {
    pub fn open(bus: u8, addr: u16) -> Result<Self> {
        let path = format!("/dev/i2c-{bus}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0)
            .open(path)?;
        Ok(Self {
            file,
            addr,
            chunk_delay: Duration::from_millis(1),
            initial_wait: Duration::from_millis(10),
            next_transfer_id: 1,
        })
    }

    pub fn set_chunk_delay(&mut self, delay: Duration) {
        self.chunk_delay = delay;
    }

    pub fn set_initial_wait(&mut self, delay: Duration) {
        self.initial_wait = delay;
    }

    pub fn send_data(&mut self, payload: &[u8]) -> Result<()> {
        self.write_request(REQ_DATA_MAGIC, payload)
    }

    pub fn send_command(&mut self, command: &str) -> Result<String> {
        let mut line = command.as_bytes().to_vec();
        if !line.ends_with(b"\n") {
            line.push(b'\n');
        }
        self.write_request(REQ_COMMAND_MAGIC, &line)?;
        thread::sleep(self.initial_wait);
        for _ in 0..50 {
            match self.read_frame()? {
                Frame::Command(bytes) => {
                    return Ok(String::from_utf8_lossy(&bytes).into_owned());
                }
                Frame::Data(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        Err(Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for Pico command response",
        )))
    }

    pub fn poll_once(&mut self) -> Result<Option<Vec<u8>>> {
        match self.read_frame()? {
            Frame::Data(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
            Frame::Data(_) => Ok(None),
            Frame::Command(_) => Ok(None),
        }
    }

    pub fn stream<F>(&mut self, poll_interval: Duration, mut on_frame: F) -> Result<()>
    where
        F: FnMut(Frame),
    {
        loop {
            let frame = self.read_frame()?;
            match &frame {
                Frame::Data(bytes) if bytes.is_empty() => {}
                _ => on_frame(frame),
            }
            thread::sleep(poll_interval);
        }
    }

    fn write_request(&mut self, magic: u8, payload: &[u8]) -> Result<()> {
        let payload = &payload[..payload.len().min(PAYLOAD_MAX)];
        let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + payload.len());
        frame.push(magic);
        frame.push(if magic == REQ_COMMAND_MAGIC {
            RESP_COMMAND_MAGIC
        } else {
            RESP_DATA_MAGIC
        });
        frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        frame.extend_from_slice(payload);

        let transfer_id = self.next_transfer_id;
        self.next_transfer_id = self.next_transfer_id.wrapping_add(1).max(1);
        let kind = if magic == REQ_COMMAND_MAGIC {
            KIND_COMMAND
        } else {
            KIND_DATA
        };
        for slot in encode_slots(kind, transfer_id, &frame) {
            self.transfer_write(&slot)?;
            if frame.len() > SLOT_PAYLOAD_SIZE {
                thread::sleep(self.chunk_delay);
            }
        }
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let mut active = false;
        let mut kind = KIND_IDLE;
        let mut transfer_id = 0u16;
        let mut total_len = 0usize;
        let mut next_offset = 0usize;
        let mut raw = Vec::new();

        loop {
            let mut slot = [0u8; SLOT_SIZE];
            self.transfer_read(&mut slot)?;
            if let Some(decoded) = decode_slot(&slot)? {
                if (decoded.flags & FLAG_START) != 0 {
                    active = true;
                    kind = decoded.kind;
                    transfer_id = decoded.transfer_id;
                    total_len = decoded.total_len;
                    next_offset = decoded.data.len();
                    raw.clear();
                    raw.extend_from_slice(&decoded.data);
                } else if active
                    && decoded.kind == kind
                    && decoded.transfer_id == transfer_id
                    && decoded.offset == next_offset
                {
                    next_offset += decoded.data.len();
                    raw.extend_from_slice(&decoded.data);
                } else {
                    return Err(Error::InvalidFrame {
                        magic: decoded.kind,
                        len: decoded.total_len,
                    });
                }

                if (decoded.flags & FLAG_END) != 0 {
                    if next_offset != total_len {
                        return Err(Error::InvalidFrame {
                            magic: kind,
                            len: total_len,
                        });
                    }
                    return parse_response(kind, &raw);
                }
            }
            thread::sleep(self.chunk_delay);
        }
    }

    fn transfer_write(&mut self, data: &[u8]) -> Result<()> {
        let mut msg = I2cMsg {
            addr: self.addr,
            flags: 0,
            len: data.len() as u16,
            buf: data.as_ptr() as *mut u8,
        };
        self.transfer_ioctl(&mut msg)
    }

    fn transfer_read(&mut self, data: &mut [u8]) -> Result<()> {
        let mut msg = I2cMsg {
            addr: self.addr,
            flags: I2C_M_RD,
            len: data.len() as u16,
            buf: data.as_mut_ptr(),
        };
        self.transfer_ioctl(&mut msg)
    }

    fn transfer_ioctl(&mut self, msg: &mut I2cMsg) -> Result<()> {
        let mut ioctl_data = I2cRdwrIoctlData {
            msgs: msg as *mut _,
            nmsgs: 1,
        };

        let rc = unsafe { ioctl(self.file.as_raw_fd(), I2C_RDWR, &mut ioctl_data as *mut _) };
        if rc < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}

fn parse_response(slot_kind: u8, raw: &[u8]) -> Result<Frame> {
    if raw.len() < FRAME_HEADER_SIZE {
        return Err(Error::InvalidFrame {
            magic: slot_kind,
            len: raw.len(),
        });
    }
    let magic = match (raw[0], raw[1]) {
        (REQ_DATA_MAGIC, RESP_DATA_MAGIC) => RESP_DATA_MAGIC,
        (REQ_COMMAND_MAGIC, RESP_COMMAND_MAGIC) => RESP_COMMAND_MAGIC,
        _ => slot_kind,
    };
    let len = u16::from_le_bytes([raw[2], raw[3]]) as usize;
    if len > PAYLOAD_MAX {
        return Err(Error::InvalidFrame { magic, len });
    }
    let payload_end = FRAME_HEADER_SIZE + len;
    if payload_end > raw.len() {
        return Err(Error::InvalidFrame { magic, len });
    }

    let payload = raw[FRAME_HEADER_SIZE..payload_end].to_vec();
    match magic {
        RESP_DATA_MAGIC => Ok(Frame::Data(payload)),
        RESP_COMMAND_MAGIC => Ok(Frame::Command(payload)),
        _ => Err(Error::InvalidFrame { magic, len }),
    }
}

struct DecodedSlot {
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: usize,
    total_len: usize,
    data: Vec<u8>,
}

fn encode_slots(kind: u8, transfer_id: u16, payload: &[u8]) -> Vec<[u8; SLOT_SIZE]> {
    let mut out = Vec::new();
    let total_len = payload.len();
    let mut offset = 0usize;
    loop {
        let end = (offset + SLOT_PAYLOAD_SIZE).min(total_len);
        let chunk = &payload[offset..end];
        let mut flags = 0u8;
        if offset == 0 {
            flags |= FLAG_START;
        }
        if end >= total_len {
            flags |= FLAG_END;
        }
        out.push(encode_slot(
            kind,
            flags,
            transfer_id,
            offset,
            total_len,
            chunk,
        ));
        if end >= total_len {
            break;
        }
        offset = end;
    }
    out
}

fn encode_slot(
    kind: u8,
    flags: u8,
    transfer_id: u16,
    offset: usize,
    total_len: usize,
    data: &[u8],
) -> [u8; SLOT_SIZE] {
    let mut raw = [0u8; SLOT_SIZE];
    raw[0] = SLOT_MAGIC0;
    raw[1] = SLOT_MAGIC1;
    raw[2] = SLOT_VERSION;
    raw[3] = kind;
    raw[4] = flags;
    raw[6..10].copy_from_slice(&(offset as u32).to_le_bytes());
    raw[10..14].copy_from_slice(&(total_len as u32).to_le_bytes());
    raw[14..16].copy_from_slice(&(data.len() as u16).to_le_bytes());
    raw[16..18].copy_from_slice(&transfer_id.to_le_bytes());
    raw[SLOT_HEADER_SIZE..SLOT_HEADER_SIZE + data.len()].copy_from_slice(data);
    raw
}

fn decode_slot(raw: &[u8; SLOT_SIZE]) -> Result<Option<DecodedSlot>> {
    if raw.iter().all(|&byte| byte == 0x00)
        || raw.iter().all(|&byte| byte == 0xFF)
        || raw[3] == KIND_IDLE
    {
        return Ok(None);
    }
    if raw[0] != SLOT_MAGIC0 || raw[1] != SLOT_MAGIC1 || raw[2] != SLOT_VERSION {
        return Err(Error::InvalidFrame {
            magic: raw[0],
            len: raw.len(),
        });
    }
    let data_len = u16::from_le_bytes([raw[14], raw[15]]) as usize;
    if data_len > SLOT_PAYLOAD_SIZE {
        return Err(Error::InvalidFrame {
            magic: raw[3],
            len: data_len,
        });
    }
    Ok(Some(DecodedSlot {
        kind: raw[3],
        flags: raw[4],
        transfer_id: u16::from_le_bytes([raw[16], raw[17]]),
        offset: u32::from_le_bytes([raw[6], raw[7], raw[8], raw[9]]) as usize,
        total_len: u32::from_le_bytes([raw[10], raw[11], raw[12], raw[13]]) as usize,
        data: raw[SLOT_HEADER_SIZE..SLOT_HEADER_SIZE + data_len].to_vec(),
    }))
}

#[allow(dead_code)]
fn _assert_c_layouts() {
    let _ = MaybeUninit::<I2cMsg>::uninit();
    let _ = MaybeUninit::<I2cRdwrIoctlData>::uninit();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_frame() {
        let mut raw = Vec::new();
        raw.push(REQ_COMMAND_MAGIC);
        raw.push(RESP_COMMAND_MAGIC);
        raw.extend_from_slice(&4u16.to_le_bytes());
        raw.extend_from_slice(b"pong");
        assert_eq!(
            parse_response(KIND_COMMAND, &raw).unwrap(),
            Frame::Command(b"pong".to_vec())
        );
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(
            parse_response(KIND_ERROR, &[]),
            Err(Error::InvalidFrame {
                magic: KIND_ERROR,
                len: 0
            })
        ));
    }
}
