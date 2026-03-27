//! Linux host-side SPI backend for Pico-Fi framed transport.
//!
//! This module uses `/dev/spidev*` and `SPI_IOC_MESSAGE` directly so a
//! groundstation can talk to the Pico-Fi SPI slave without Python.

use std::fmt;
use std::fs::OpenOptions;
use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::thread;
use std::time::Duration;

const FRAME_SIZE: usize = 258;
const PAYLOAD_MAX: usize = FRAME_SIZE - 2;
const REQ_DATA_MAGIC: u8 = 0xA5;
const REQ_COMMAND_MAGIC: u8 = 0xA6;
const RESP_DATA_MAGIC: u8 = 0x5A;
const RESP_COMMAND_MAGIC: u8 = 0x5B;

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u32 = 1;
const SPI_IOC_MAGIC: u32 = b'k' as u32;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> c_ulong {
    ((dir << IOC_DIRSHIFT)
        | (ty << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)) as c_ulong
}

const fn iow<T>(ty: u32, nr: u32) -> c_ulong {
    ioc(IOC_WRITE, ty, nr, size_of::<T>() as u32)
}

const SPI_IOC_WR_MODE: c_ulong = iow::<u8>(SPI_IOC_MAGIC, 1);
const SPI_IOC_WR_BITS_PER_WORD: c_ulong = iow::<u8>(SPI_IOC_MAGIC, 3);
const SPI_IOC_WR_MAX_SPEED_HZ: c_ulong = iow::<u32>(SPI_IOC_MAGIC, 4);

#[repr(C)]
struct SpiIocTransfer {
    tx_buf: u64,
    rx_buf: u64,
    len: u32,
    speed_hz: u32,
    delay_usecs: u16,
    bits_per_word: u8,
    cs_change: u8,
    tx_nbits: u8,
    rx_nbits: u8,
    word_delay_usecs: u8,
    pad: u8,
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
                write!(f, "invalid SPI frame: magic=0x{magic:02x} len={len}")
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

pub struct SpiBackend {
    file: std::fs::File,
    speed_hz: u32,
    poll_delay: Duration,
}

impl SpiBackend {
    pub fn open(path: &str, speed_hz: u32) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut backend = Self {
            file,
            speed_hz,
            poll_delay: Duration::from_millis(10),
        };
        backend.configure()?;
        Ok(backend)
    }

    pub fn set_poll_delay(&mut self, delay: Duration) {
        self.poll_delay = delay;
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

        for _ in 0..50 {
            match self.read_frame()? {
                Frame::Command(bytes) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Frame::Data(_) => thread::sleep(self.poll_delay),
            }
        }

        Err(Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for Pico SPI command response",
        )))
    }

    pub fn poll_once(&mut self) -> Result<Option<Vec<u8>>> {
        match self.read_frame()? {
            Frame::Data(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
            Frame::Data(_) | Frame::Command(_) => Ok(None),
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

    fn configure(&mut self) -> Result<()> {
        let mut mode = 0u8;
        let mut bits = 8u8;
        let mut speed = self.speed_hz;
        ioctl_write(self.file.as_raw_fd(), SPI_IOC_WR_MODE, &mut mode)?;
        ioctl_write(self.file.as_raw_fd(), SPI_IOC_WR_BITS_PER_WORD, &mut bits)?;
        ioctl_write(self.file.as_raw_fd(), SPI_IOC_WR_MAX_SPEED_HZ, &mut speed)?;
        Ok(())
    }

    fn write_request(&mut self, magic: u8, payload: &[u8]) -> Result<()> {
        let frame = build_request_frame(magic, payload);
        let mut sink = [0u8; FRAME_SIZE];
        self.transfer(&frame, &mut sink)?;
        Ok(())
    }

    fn read_frame(&mut self) -> Result<Frame> {
        let mut raw = [0u8; FRAME_SIZE];
        let tx = [0u8; FRAME_SIZE];
        self.transfer(&tx, &mut raw)?;
        parse_response(&raw)
    }

    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<()> {
        debug_assert_eq!(tx.len(), rx.len());
        let mut transfer = SpiIocTransfer {
            tx_buf: tx.as_ptr() as u64,
            rx_buf: rx.as_mut_ptr() as u64,
            len: tx.len() as u32,
            speed_hz: self.speed_hz,
            delay_usecs: 0,
            bits_per_word: 8,
            cs_change: 0,
            tx_nbits: 0,
            rx_nbits: 0,
            word_delay_usecs: 0,
            pad: 0,
        };
        let request = spi_ioc_message(1);
        let rc = unsafe { ioctl(self.file.as_raw_fd(), request, &mut transfer as *mut _) };
        if rc < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(())
    }
}

fn spi_ioc_message(n: usize) -> c_ulong {
    ioc(
        IOC_WRITE,
        SPI_IOC_MAGIC,
        0,
        (size_of::<SpiIocTransfer>() * n) as u32,
    )
}

fn ioctl_write<T>(fd: c_int, request: c_ulong, value: &mut T) -> Result<()> {
    let rc = unsafe { ioctl(fd, request, value as *mut T) };
    if rc < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    Ok(())
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

fn build_request_frame(magic: u8, payload: &[u8]) -> [u8; FRAME_SIZE] {
    let payload = &payload[..payload.len().min(PAYLOAD_MAX)];
    let mut frame = [0u8; FRAME_SIZE];
    frame[0] = magic;
    frame[1] = payload.len() as u8;
    frame[2..2 + payload.len()].copy_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spi_command_frame() {
        let mut raw = [0u8; FRAME_SIZE];
        raw[0] = RESP_COMMAND_MAGIC;
        raw[1] = 4;
        raw[2..6].copy_from_slice(b"pong");
        assert_eq!(parse_response(&raw).unwrap(), Frame::Command(b"pong".to_vec()));
    }

    #[test]
    fn builds_full_sized_request_frame() {
        let frame = build_request_frame(REQ_COMMAND_MAGIC, b"/ping\n");
        assert_eq!(frame.len(), FRAME_SIZE);
        assert_eq!(frame[0], REQ_COMMAND_MAGIC);
        assert_eq!(frame[1], 6);
        assert_eq!(&frame[2..8], b"/ping\n");
        assert!(frame[8..].iter().all(|&byte| byte == 0));
    }
}
