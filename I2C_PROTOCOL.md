# I2C Protocol

This project uses a framed I2C transport between a Linux I2C master and the Pico acting as an `I2C0` slave at address `0x55`.

Unlike UART and SPI, the I2C host transport is chunked into fixed 32-byte slots on the wire.

## Electrical Setup

The Pico uses:

- `GPIO0` = `I2C0 SDA`
- `GPIO1` = `I2C0 SCL`

Pi wiring:

- Pi `SDA` -> Pico `GPIO0`
- Pi `SCL` -> Pico `GPIO1`
- Pi `GND` -> Pico `GND`

## Slot Format

The host-side I2C transport uses `32` byte slots.

Constants from [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py):

- slot size = `32`
- header size = `18`
- payload per slot = `14`
- magic bytes = `0x49 0x32`
- version = `1`

Each slot contains:

- magic/version
- kind
- flags (`START`, `END`)
- transfer id
- transfer offset
- total payload length
- slot payload bytes

Complete logical messages are reassembled from one or more slots.

## Message Semantics

Logical payloads still use the same application-level meanings as the other backends:

- `KIND_DATA` carries bridged data
- `KIND_COMMAND` carries local Pico commands such as `/ping`, `/show`, and `/link`
- empty `KIND_DATA` polls are used to fetch pending bridged data

## Host Tools

Test tool:

```bash
python3 host/python/i2c/test.py --bus 1 probe --count 3
python3 host/python/i2c/test.py --bus 1 command /ping
python3 host/python/i2c/test.py --bus 1 command /link
```

Current note:

- the I2C helper currently exposes `probe` and `command`
- there is no standalone `send`/`recv` helper yet in `host/python/i2c/test.py`
- bridged data is easiest to exercise through `link_terminal.py` or the `sedsprintf` router

Interactive terminal:

```bash
python3 host/python/i2c/link_terminal.py --bus 1 --addr 0x55
```

Terminal behavior:

- plain text lines are sent as bridged data
- `/...` lines are sent as local Pico commands
- outbound chat is rendered as `sender: message`

## sedsprintf Router

There is also an I2C router that wraps UDP datagrams in `sedsprintf_rs_2026` packets and carries them over the I2C Fi link.

Example:

```bash
python3 host/python/i2c/sedsprintf_router.py \
  --bus 1 \
  --addr 0x55 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender i2c-end
```

Paired example:

```bash
python3 host/python/i2c/sedsprintf_router.py \
  --bus 1 \
  --addr 0x55 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender i2c-end

python3 host/python/uart/sedsprintf_router.py \
  --port /dev/ttyUSB0 \
  --listen-port 9001 \
  --forward-port 9000 \
  --sender uart-end
```

## References

- [host/python/i2c/protocol.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/protocol.py)
- [host/python/i2c/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/test.py)
- [host/python/i2c/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/link_terminal.py)
- [host/python/i2c/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/sedsprintf_router.py)
- [src/bridge/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c.rs)
