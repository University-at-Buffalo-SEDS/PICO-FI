# UART Protocol

This project uses `UART0` as a framed upstream transport after the short boot-time shell exits.

Runtime UART uses fixed-size binary frames. It is not newline-delimited text mode.

## Physical Link

Default UART settings:

- `UART0`
- `115200` baud
- `8N1`
- no software or hardware flow control

Pins:

- `GPIO0` = UART TX from Pico
- `GPIO1` = UART RX into Pico

For a USB-UART adapter:

- adapter `TX` -> Pico `GPIO1`
- adapter `RX` -> Pico `GPIO0`
- adapter `GND` -> Pico `GND`

## Runtime Frame Format

All runtime UART traffic is sent as fixed `258` byte frames:

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

Semantics:

- `0xA5` requests carry bridged data.
- `0xA6` requests carry local Pico commands such as `/ping`, `/show`, and `/link`.
- `0x5A` responses carry bridged data or an empty poll reply.
- `0x5B` responses carry local Pico command replies.

## Boot Window

Immediately after reset, UART is briefly attached to the boot/config shell for about `3000 ms`.

During that window the Pico can emit plain ASCII lines. After startup finishes, UART switches to framed runtime packets.

Implications for host tools:

- if the Pico has just reset, wait a few seconds before starting framed traffic
- or explicitly drive the shell to runtime mode first if you are using the boot window intentionally

## Host Tools

Test tool:

```bash
python3 host/python/uart/test.py --port /dev/ttyUSB0 probe --count 3
python3 host/python/uart/test.py --port /dev/ttyUSB0 command /ping
python3 host/python/uart/test.py --port /dev/ttyUSB0 send "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 recv --expect "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 data "hello" --expect "hello"
```

Typical bridge check:

```bash
python3 host/python/uart/test.py --port /dev/ttyUSB0 send "uart-to-peer"
python3 host/python/uart/test.py --port /dev/ttyUSB0 recv --expect "peer-to-uart"
```

Interactive terminal:

```bash
python3 host/python/uart/link_terminal.py --port /dev/ttyUSB0
```

Terminal behavior:

- plain text lines are sent as bridged data
- `/...` lines are sent as local Pico commands
- outbound chat is rendered as `sender: message`

## sedsprintf Router

There is also a UART router that wraps UDP datagrams in `sedsprintf_rs_2026` packets and carries them over the UART Fi link.

Example:

```bash
python3 host/python/uart/sedsprintf_router.py \
  --port /dev/ttyUSB0 \
  --speed 115200 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender uart-end
```

Paired example:

```bash
python3 host/python/uart/sedsprintf_router.py \
  --port /dev/ttyUSB0 \
  --listen-port 9000 \
  --forward-port 9001 \
  --sender uart-end

python3 host/python/spi/sedsprintf_router.py \
  --bus 0 \
  --device 0 \
  --speed 100000 \
  --listen-port 9001 \
  --forward-port 9000 \
  --sender spi-end
```

This router:

- listens on local UDP `9000`
- sends received datagrams over UART inside `sedsprintf` packets
- forwards received `sedsprintf` payloads to local UDP `9001`

## References

- [host/python/uart/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/test.py)
- [host/python/uart/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/link_terminal.py)
- [host/python/uart/sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/sedsprintf_router.py)
- [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
