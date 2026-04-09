# SPI Protocol

This project uses a framed SPI protocol between a Linux SPI master and the Pico acting as an `SPI1` slave.

The Linux master should use SPI mode `3` (`CPOL=1`, `CPHA=1`). The firmware's
PIO slave timing currently matches that mode.

## Electrical setup

The Pico upstream SPI slave uses:

- `GPIO10` = `SPI1 SCK`
- `GPIO11` = `SPI1 TX` output from Pico
- `GPIO12` = `SPI1 RX` input to Pico
- `GPIO13` = `SPI1 CSn`

Pi wiring:

- Pi `SCLK` -> Pico `GPIO10`
- Pi `MOSI` -> Pico `GPIO12`
- Pi `MISO` <- Pico `GPIO11`
- Pi `CE0`/chip-select -> Pico `GPIO13`
- Pi `GND` -> Pico `GND`

## Frame layout

Requests are sent as fixed `258` byte SPI transactions so they stay within a
single Linux `spidev` chip-select window:

- byte `0`: magic
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Responses are clocked out as fixed `258` byte frames:

- byte `0`: magic
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Constants:

- frame size = `258`
- max payload = `256`
- data request magic = `0xA5`
- command request magic = `0xA6`
- data response magic = `0x5A`
- command response magic = `0x5B`

## Semantics

- `0xA5` requests carry raw bridged data.
- `0xA6` requests carry local Pico commands such as `/ping` and `/show`.
- Each request must fit in one CS-bounded transfer. Splitting a frame across
  multiple Linux `SPI_IOC_MESSAGE` calls will be seen by the Pico as multiple
  independent transactions.
- SPI response polling is done by issuing a full-frame readback transfer with
  zero-filled MOSI bytes.

## References

- Interactive SPI terminal: [host/python/spi/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/link_terminal.py)
- SPI test tool: [host/python/spi/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test.py)
- Firmware SPI bridge: [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs)
- Firmware SPI task: [src/bridge/spi_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_task.rs)
