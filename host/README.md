Host-side Pico-Fi I2C backend

Files:
- `i2c_backend.rs`: std-based Linux `/dev/i2c-*` backend for the Pico-Fi framed I2C transport.
- `spi_backend.rs`: std-based Linux `/dev/spidev*` backend for the Pico-Fi framed SPI transport.
- `python/i2c/`: I2C Python tools (`test.py`, `link_terminal.py`, `detect.py`, `bus_check.py`).
- `python/spi/`: SPI Python tools (`test.py`, `link_terminal.py`).
- `python/uart/`: UART Python tools (`test.py`, `link_terminal.py`).

Usage from another Rust crate:

```rust
#[path = "/absolute/path/to/pico-fi/host/i2c_backend.rs"]
mod i2c_backend;

use i2c_backend::{Frame, I2cBackend};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut backend = I2cBackend::open(1, 0x55)?;

    let pong = backend.send_command("/ping")?;
    println!("pico replied: {pong}");

    backend.send_data(b"frontend hello\n")?;

    backend.stream(Duration::from_millis(20), |frame| match frame {
        Frame::Data(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            println!("stream: {text}");
        }
        Frame::Command(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            println!("command: {text}");
        }
    })?;

    Ok(())
}
```

Notes:
- This module is for Linux hosts. It uses the `I2C_RDWR` ioctl directly.
- `Frame::Data` is the streaming payload you would forward into the frontend.
- `Frame::Command` is for local Pico command replies such as `/ping` and `/link`.
- The backend mirrors the Python terminal behavior: writes are chunked to 32 bytes and reads pull a full 258-byte response frame.

SPI notes:
- The SPI backend uses `SPI_IOC_MESSAGE` on `/dev/spidev*`.
- The Pico SPI slave path uses `SPI1` on `GPIO10`=`SCK`, `GPIO11`=`MOSI`, `GPIO12`=`MISO`, `GPIO13`=`CSn`.
- SPI reads are implemented as full-duplex transfers with zero-filled MOSI bytes while clocking data out of the Pico.

Example Python entrypoints:
- `python3 -m host.python.i2c.test command /ping`
- `python3 -m host.python.i2c.link_terminal`
- `python3 -m host.python.spi.test command /ping`
- `python3 -m host.python.spi.link_terminal`
- `python3 -m host.python.uart.test command /ping --port /dev/ttyUSB0`
