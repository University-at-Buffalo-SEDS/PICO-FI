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
const FRAME_SIZE: usize = 258;
const PAYLOAD_MAX: usize = FRAME_SIZE - 2;
const CHUNK_SIZE: usize = 32;

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
        let mut frame = Vec::with_capacity(2 + payload.len());
        frame.push(magic);
        frame.push(payload.len() as u8);
        frame.extend_from_slice(payload);

        for chunk in frame.chunks(CHUNK_SIZE) {
            self.transfer_write(chunk)?;
            if chunk.len() == CHUNK_SIZE {
                thread::sleep(self.chunk_delay);
            }
        }
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let mut raw = [0u8; FRAME_SIZE];
        let mut offset = 0usize;
        while offset < FRAME_SIZE {
            let read_len = (FRAME_SIZE - offset).min(CHUNK_SIZE);
            let dst = &mut raw[offset..offset + read_len];
            self.transfer_read(dst)?;
            offset += read_len;
            if offset < FRAME_SIZE {
                thread::sleep(self.chunk_delay);
            }
        }
        parse_response(&raw)
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

fn parse_response(raw: &[u8; FRAME_SIZE]) -> Result<Frame> {
    let magic = raw[0];
    let len = raw[1] as usize;
    if len > PAYLOAD_MAX {
        return Err(Error::InvalidFrame { magic, len });
    }

    let payload = raw[2..2 + len].to_vec();
    match magic {
        RESP_DATA_MAGIC => Ok(Frame::Data(payload)),
        RESP_COMMAND_MAGIC => Ok(Frame::Command(payload)),
        _ => Err(Error::InvalidFrame { magic, len }),
    }
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
        let mut raw = [0u8; FRAME_SIZE];
        raw[0] = RESP_COMMAND_MAGIC;
        raw[1] = 4;
        raw[2..6].copy_from_slice(b"pong");
        assert_eq!(parse_response(&raw).unwrap(), Frame::Command(b"pong".to_vec()));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut raw = [0u8; FRAME_SIZE];
        raw[0] = 0x00;
        raw[1] = 0;
        assert!(matches!(
            parse_response(&raw),
            Err(Error::InvalidFrame { magic: 0x00, len: 0 })
        ));
    }
}
