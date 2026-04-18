# SPI Protocol

This document describes the current Pico-Fi SPI upstream transport as implemented by:

- [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs)
- [src/bridge/spi_hw_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_hw_task.rs)
- [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs)
- [host/python/spi/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test.py)

The Linux side is always the SPI master. The Pico acts as an `SPI1` slave.

## Electrical Setup

SPI mode requirements:

- mode `3`
- `CPOL=1`
- `CPHA=1`
- 8-bit words

Pico pins:

- `GPIO10`: `SPI1 SCK`
- `GPIO11`: Pico TX / MISO from Pico
- `GPIO12`: Pico RX / MOSI into Pico
- `GPIO13`: `SPI1 CSn`

Typical Raspberry Pi wiring:

- Pi `SCLK` -> Pico `GPIO10`
- Pi `MOSI` -> Pico `GPIO12`
- Pi `MISO` <- Pico `GPIO11`
- Pi `CE0` -> Pico `GPIO13`
- Pi `GND` -> Pico `GND`

Important constraint:

- only one SPI client may access `/dev/spidev*` at a time

## Transfer Format

Each SPI request/response frame is encoded as `header + payload`:

- byte `0`: request magic
- byte `1`: response/sync magic
- bytes `2..3`: payload length `N`, little-endian `u16`
- bytes `4..(4+N)`: payload

Constants:

- maximum frame buffer: `260`
- header size: `4`
- max payload: `256`
- request data magic: `0xA5`
- request command magic: `0xA6`
- response data magic: `0x5A`
- response command magic: `0x5B`

Data frames use the sync pair `0xA5 0x5A`. Command frames use `0xA6 0x5B`.

One Pico response is returned on the same full-duplex transaction, or on a later poll if the response is queued. The
Pico stages only the active `header + payload_len` bytes; host tools may still clock a larger poll transaction when
they do not know the queued response length in advance.

## Semantics

`0xA5` request:

- carries bridged data
- empty payload is just an empty data frame and is commonly used as a probe

`0xA6` request:

- carries a local Pico command such as `/ping\n` or `/link\n`
- also carries the host-side `/pull\n` command used to fetch queued inbound data

`0x5A` response:

- carries bridged data
- or an empty acknowledgement / empty queued-data result

`0x5B` response:

- carries local Pico command output
- or an error such as `error invalid spi frame`

## Current Firmware Behavior

The SPI bridge currently accepts both of these bridged-data paths:

- `0xA5` with arbitrary payload
- `0xA6` with non-slash payload

The host tools use:

- `0xA5` for normal data sends
- `0xA6 "/pull\n"` to fetch queued inbound data
- `0xA6 "/... \n"` for local Pico commands

The relevant logic is in [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs).

Queue behavior:

- SPI ingress and response queues drop whole frames, never individual bytes
- each queue is capped at `32` frames and `8192` queued frame bytes
- queued frames store only the active `header + payload` bytes
- if a new frame would exceed the byte cap, old frames are dropped until it fits
- if a single frame is larger than the byte cap, that frame is dropped

## Minimal Driver Algorithm

To send bridged data:

1. Build a frame with sync bytes `0xA5 0x5A`.
2. Write the payload length as little-endian `u16`.
3. Copy the payload into bytes `4..`.
4. Perform one SPI transfer containing the active `4 + N` bytes.
5. Parse any returned response frame.

To fetch pending inbound data:

1. Send a `0xA6` frame whose payload is `/pull\n`.
2. If the same transfer returns `0x5A` with non-zero length, consume it.
3. Otherwise keep issuing poll transfers until a queued `0x5A` with data arrives or your timeout
   expires.

To issue a local Pico command:

1. Send a `0xA6` frame containing `/command\n`.
2. Poll until a `0x5B` response arrives.

## Example Frames

Send bridged payload `hello`:

```text
a5 5a 05 00 68 65 6c 6c 6f
```

Poll inbound queue:

```text
a6 5b 06 00 2f 70 75 6c 6c 0a
```

Send local command `/link\n`:

```text
a6 5b 06 00 2f 6c 69 6e 6b 0a
```

## Response Timing Model

SPI is the most stateful host transport in this repo.

Practical rules for driver authors:

- treat every transaction as request plus possible reply
- keep chip select asserted for each complete `header + payload` frame
- do not split a single frame across multiple SPI transactions
- be prepared to poll after sends to retrieve queued inbound data or delayed command replies

The current Python reference implementation is in:

- [host/python/spi/raw.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/raw.py)
- [host/python/spi/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test.py)

## Host Tools

Raw helper:

```bash
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 probe --count 3
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 command /link
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 send "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 recv --expect "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 data "hello" --expect "hello"
```

Interactive terminal:

```bash
python3 host/python/spi/link_terminal.py --bus 0 --device 0 --speed 100000
```

Telemetry packet validation:

```bash
python3 host/python/telemetry_terminal.py --sender spi-node spi --bus 0 --device 0 --speed 100000
```

Telemetry decode note:

- the telemetry helpers accept both `SP6:`-armored packets and raw serialized `sedsprintf_rs_2026` packets on receive
- the terminal renders the decoded packet using the packet library string conversion when available

## Diagnostics

The SPI firmware includes diagnostic and recovery support in:

- [src/bridge/spi_diag.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_diag.rs)
- [src/bridge/spi_hw_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_hw_task.rs)

Malformed captures are failed closed and can produce:

- empty `0x5A`
- `0x5B "error invalid spi frame"`

## sedsprintf Router

The router wraps UDP payloads in `sedsprintf_rs_2026` serialized packets and sends those bytes over the normal SPI
framed data path.

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

Current router note:

- the shared router path currently sends `SP6:`-armored packets
- the telemetry CLI and telemetry terminal can also decode raw serialized packets on receive

This does not change the SPI wire format. It only changes the payload bytes placed inside `0xA5` data frames.

## References

- [host/python/spi/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test.py)
- [host/python/spi/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/link_terminal.py)
- [host/python/spi/raw.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/raw.py)
- [host/python/spi/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/sedsprintf_router.py)
- [host/python/telemetry_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/telemetry_terminal.py)
- [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs)
- [src/bridge/spi_hw_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi_hw_task.rs)
- [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs)
