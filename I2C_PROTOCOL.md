# I2C Protocol

This document describes the current Pico-Fi I2C upstream transport as implemented by:

- [src/bridge/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c.rs)
- [src/bridge/i2c_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c_task.rs)
- [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py)
- [host/python/i2c/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/test.py)

Unlike UART and SPI, I2C does not use one contiguous wire frame. It uses chunked `32` byte slots that are reassembled
into logical packets. Those logical packets carry the same `4` byte request/response header used by UART and SPI.

## Electrical Setup

Default bus role:

- Linux host: I2C master
- Pico: `I2C0` slave
- default address: `0x55`

Pico pins:

- `GPIO0`: `I2C0 SDA`
- `GPIO1`: `I2C0 SCL`

Typical Pi wiring:

- Pi `SDA` -> Pico `GPIO0`
- Pi `SCL` -> Pico `GPIO1`
- Pi `GND` -> Pico `GND`

## Slot Format

Each on-wire transfer is one `32` byte slot.

Constants from [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py):

- slot size: `32`
- header size: `18`
- payload bytes per slot: `14`
- magic: `0x49 0x32`
- version: `1`

Per-slot layout:

- bytes `0..1`: magic
- byte `2`: version
- byte `3`: kind
- byte `4`: flags
- byte `5`: reserved
- bytes `6..9`: payload offset, little-endian
- bytes `10..13`: total transfer length, little-endian
- bytes `14..15`: slot payload length, little-endian
- bytes `16..17`: transfer id, little-endian
- bytes `18..31`: slot payload bytes

Flags:

- `FLAG_START = 0x01`
- `FLAG_END = 0x02`

Large logical messages are split across multiple slots and reassembled by transfer id plus offset.

## Packet Kinds

Current host and firmware kinds:

- `KIND_IDLE = 0x00`
- `KIND_DATA = 0x01`
- `KIND_COMMAND = 0x02`
- `KIND_ERROR = 0x7F`

Semantics:

- `KIND_DATA`: bridged data
- `KIND_COMMAND`: local Pico commands such as `/ping`, `/show`, `/link`
- `KIND_ERROR`: protocol or parsing error

An empty `KIND_DATA` payload is the I2C-side equivalent of a poll / empty acknowledgement.

## Current Firmware Behavior

When the bridge session is active:

- non-empty `KIND_DATA` payloads are forwarded across the bridge
- incoming network data is queued back to the I2C host as `KIND_DATA`
- `KIND_COMMAND` is handled locally on the Pico

When the bridge session is not active:

- `KIND_COMMAND` still works locally
- non-empty `KIND_DATA` may receive only an empty acknowledgement

Unknown kinds are answered with:

- `KIND_ERROR`
- payload `error invalid i2c frame`

Queue behavior:

- I2C ingress and response queues drop whole logical packets, never individual bytes
- each queue is capped at `8` packets and `8192` queued payload bytes
- if a new packet would exceed the byte cap, old packets are dropped until it fits
- if a single packet is larger than the byte cap, that packet is dropped

## Minimal Driver Algorithm

To send a logical packet:

1. Split the payload into `14` byte chunks.
2. Emit one or more `32` byte slots with:
3. matching transfer id
4. offset increasing by chunk length
5. `FLAG_START` on the first slot
6. `FLAG_END` on the last slot

To receive a logical packet:

1. Read `32` byte slots from the slave.
2. Ignore all-zero / all-`0xFF` / `KIND_IDLE` slots.
3. Start a new assembly when `FLAG_START` is set.
4. Append payload bytes while validating:
5. same kind
6. same transfer id
7. matching offset
8. stop when `FLAG_END` arrives and total length matches

The reference implementation for this is already in:

- [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py)

If you are implementing another driver, copy that state machine behavior.

## Example Host Usage

Current `host/python/i2c/test.py` supports:

```bash
python3 host/python/i2c/test.py --bus 1 --addr 0x55 probe --count 3
python3 host/python/i2c/test.py --bus 1 --addr 0x55 command /ping
python3 host/python/i2c/test.py --bus 1 --addr 0x55 command /link
```

Current note:

- the I2C test helper does not yet expose standalone `send` / `recv` subcommands like UART and SPI
- the interactive terminal and telemetry router are the easiest current ways to drive bridged I2C payloads

Interactive terminal:

```bash
python3 host/python/i2c/link_terminal.py --bus 1 --addr 0x55
```

Telemetry note:

- the current telemetry terminal only supports UART and SPI
- for I2C telemetry validation today,
  use [host/python/i2c/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/sedsprintf_router.py)
  or add an I2C backend that
  reuses [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py)

## sedsprintf Router

The I2C router wraps UDP datagrams in `sedsprintf_rs_2026` packets and sends those serialized bytes inside logical
`KIND_DATA` packets.

Example:

```bash
python3 host/python/i2c/sedsprintf_router.py \
  --bus 1 \
  --addr 0x55 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender i2c-end
```

Again, this does not change the I2C slot format. It only changes the bytes carried in the logical data packet.

## References

- [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py)
- [host/python/i2c/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/test.py)
- [host/python/i2c/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/link_terminal.py)
- [host/python/i2c/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/sedsprintf_router.py)
- [src/bridge/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c.rs)
- [src/bridge/i2c_task.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c_task.rs)
