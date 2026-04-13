# UART Protocol

This document describes the current Pico-Fi UART upstream transport as implemented by:

- [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs)
- [host/python/uart/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/test.py)

The UART runtime protocol is a fixed-size binary frame protocol. It is not line-oriented text mode.

## Physical Link

Default settings:

- `UART0`
- `115200` baud
- `8N1`
- no RTS/CTS
- no XON/XOFF

Pins:

- `GPIO0`: Pico TX
- `GPIO1`: Pico RX

USB-UART wiring:

- adapter `TX` -> Pico `GPIO1`
- adapter `RX` -> Pico `GPIO0`
- adapter `GND` -> Pico `GND`

On macOS, use `/dev/cu.*` for host-initiated traffic rather than `/dev/tty.*`.

## Boot Window

Immediately after reset, UART is briefly attached to the boot/config shell. During that window the Pico emits plain
ASCII text.

After startup finishes, UART switches to framed runtime packets.

Practical implication:

- wait a few seconds after reset before sending framed traffic
- do not mix boot-shell text with runtime binary frames on the same open session
- the firmware now resynchronizes on valid `0xA5` and `0xA6` request starts, so a few stray shell/debug-probe bytes no
  longer permanently poison framing

## Frame Format

Every UART request and response is exactly `258` bytes:

- byte `0`: magic
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Constants:

- frame size: `258`
- max payload: `256`
- request data magic: `0xA5`
- request command magic: `0xA6`
- response data magic: `0x5A`
- response command magic: `0x5B`

The firmware parser is currently shared with the SPI/I2C framed protocol code
in [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs).

## Semantics

`0xA5` request:

- carries bridged data bytes
- empty payload is a poll for pending inbound data

`0xA6` request:

- carries a local Pico command such as `/ping\n`, `/show\n`, or `/link\n`

`0x5A` response:

- carries bridged data
- or an empty acknowledgement / empty poll result

`0x5B` response:

- carries local Pico command output
- or an error such as `error invalid uart frame`

## Current Firmware Behavior

When the Ethernet bridge session is active:

- non-empty `0xA5` payloads are forwarded across the bridge
- empty `0xA5` payloads poll for pending bridged data
- `0xA6` payloads are handled locally as Pico commands

Before the bridge session is active:

- `0xA5` requests still parse and typically receive an empty `0x5A`
- `0xA6` requests still work for local Pico commands

Important constraint:

- only one host process should own the UART device at a time

If two host tools write to the same UART simultaneously, the Pico will see corrupted framing and can reply with
`error invalid uart frame`.

## Minimal Driver Algorithm

To send bridged data:

1. Build a `258` byte frame with magic `0xA5`.
2. Put the payload length in byte `1`.
3. Copy the payload into bytes `2..`.
4. Zero-fill the rest.
5. Write all `258` bytes.
6. Read exactly `258` bytes back.

To poll for inbound bridged data:

1. Send an empty `0xA5` frame.
2. Read exactly `258` bytes back.
3. If the reply magic is `0x5A` and length is non-zero, bytes `2..(2+N)` are the inbound data.
4. If the reply magic is `0x5A` and length is zero, no data is pending.

To issue a local Pico command:

1. Send a `0xA6` frame whose payload is ASCII and usually newline-terminated, for example `/link\n`.
2. Read a `0x5B` response.

## Example Frames

Send bridged payload `hello`:

```text
a5 05 68 65 6c 6c 6f 00 00 ...
```

Poll for inbound data:

```text
a5 00 00 00 00 00 ...
```

Send local command `/ping\n`:

```text
a6 06 2f 70 69 6e 67 0a 00 ...
```

## Host Tools

Raw helper:

```bash
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 probe --count 3
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 command /ping
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 send "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 recv --expect "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 data "hello" --expect "hello"
```

Interactive terminal:

```bash
python3 host/python/uart/link_terminal.py --port /dev/ttyUSB0 --baud 115200
```

Telemetry packet validation:

```bash
python3 host/python/telemetry_terminal.py --sender uart-node uart --port /dev/ttyUSB0 --speed 115200
```

Telemetry decode note:

- the telemetry helpers accept both `SP6:`-armored packets and raw serialized `sedsprintf_rs_2026` packets on receive
- the terminal renders the decoded packet using the packet library string conversion when available

## sedsprintf Router

The router wraps UDP datagrams in `sedsprintf_rs_2026` packets and sends those serialized bytes over the UART framed
data path.

Example:

```bash
python3 host/python/uart/sedsprintf_router.py \
  --port /dev/ttyUSB0 \
  --speed 115200 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender uart-end
```

That means:

- the UART wire protocol still carries only normal `0xA5` framed payloads
- the payload bytes happen to be serialized telemetry packets
- the shared router path currently sends `SP6:`-armored packets, while the interactive telemetry tools can also decode
  raw serialized packets

## References

- [host/python/uart/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/test.py)
- [host/python/uart/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/link_terminal.py)
- [host/python/uart/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/sedsprintf_router.py)
- [host/python/telemetry_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/telemetry_terminal.py)
- [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs)
