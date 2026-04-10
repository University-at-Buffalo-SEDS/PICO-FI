# SPI Protocol

This project uses a framed SPI protocol between a Linux SPI master and the Pico acting as an `SPI1` slave.

The Linux master must use SPI mode `3` (`CPOL=1`, `CPHA=1`).

## Electrical Setup

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

## Frame Format

SPI requests and responses use fixed `258` byte framed transfers:

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

- empty `0xA5` requests are probe/poll frames
- local Pico commands use `0xA6` with payloads like `/ping\n` or `/link\n`
- plain non-empty SPI chat/data currently also uses the stable `0xA6` path on the host tools, and firmware treats non-slash `0xA6` payloads as bridged data
- `0x5A` responses carry bridged data or an empty acknowledgement
- `0x5B` responses carry local Pico command replies

Each request must fit in one CS-bounded transfer. Splitting a frame across multiple Linux SPI transactions is not supported.

## Host Tools

Test tool:

```bash
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 probe --count 3
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 command /link
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 send "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 recv --expect "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 data "hello" --expect "hello"
```

Typical bridge check:

```bash
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 send "spi-to-peer"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 recv --expect "peer-to-spi"
```

Interactive terminal:

```bash
python3 host/python/spi/link_terminal.py --bus 0 --device 0 --speed 100000
```

Terminal behavior:

- plain text lines are sent as bridged data
- `/...` lines are sent as local Pico commands
- outbound chat is rendered as `sender: message`
- only one SPI client should touch `/dev/spidev*` at a time

## Self-Healing

The SPI firmware now includes a small self-heal path:

- malformed or partial captures fail closed to an empty `0x5A` response
- repeated suspicious captures trigger a soft SPI transport reset
- self-heal no longer overwrites a real staged command reply

This is intended to make the transport recover from bad host traffic more safely instead of poisoning later transactions.

## sedsprintf Router

There is also an SPI router that wraps UDP datagrams in `sedsprintf_rs_2026` packets and carries them over the SPI Fi link.

Example:

```bash
python3 host/python/spi/sedsprintf_router.py \
  --bus 0 \
  --device 0 \
  --speed 100000 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender spi-end
```

Paired example:

```bash
python3 host/python/spi/sedsprintf_router.py \
  --bus 0 \
  --device 0 \
  --speed 100000 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender spi-end

python3 host/python/uart/sedsprintf_router.py \
  --port /dev/ttyUSB0 \
  --listen-port 9001 \
  --forward-port 9000 \
  --sender uart-end
```

## References

- [host/python/spi/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test.py)
- [host/python/spi/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/link_terminal.py)
- [host/python/spi/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/sedsprintf_router.py)
- [host/python/spi/raw.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/raw.py)
- [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs)
- [src/bridge/spi_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_task.rs)
