# UART Protocol

This document describes how to talk to the Pico over its UART upstream interface.

UART is a byte-stream transport. The Pico forwards UART bytes to the bridged TCP socket without requiring newline framing.

There is one exception: a printable line that starts with `/` at the beginning of a line is treated as a local Pico command instead of bridged data.

## Physical link

Default UART settings used by the firmware:

- UART peripheral: `UART0`
- baud: `115200`
- data bits: `8`
- parity: none
- stop bits: `1`

Pins:

- `GPIO0` = UART TX from Pico
- `GPIO1` = UART RX into Pico

## Message model

UART input is normally treated as raw bytes.

Rules:

- arbitrary binary bytes are forwarded unchanged once the TCP bridge is up
- `0x00`, escape bytes, and non-ASCII bytes are allowed
- newline is not required for bridged payload data
- TCP data received from the remote peer is written back to UART unchanged

This makes the UART path binary-safe for normal payload traffic.

## Sending commands to the Pico

To issue a local Pico command over UART:

1. Start a new line with `/`.
2. Send a printable ASCII command.
3. Terminate the line with `\n` or `\r\n`.

Supported local commands:

- `/help`
- `/show`
- `/ping`
- `/link`

These are handled locally on the Pico and are not forwarded over the Ethernet bridge.

Command recognition rules:

- command parsing only starts when `/` is the first byte on a new line
- the rest of the command must stay printable ASCII
- newline ends the command
- if a non-printable byte appears after the leading `/`, the accumulated bytes are treated as normal payload instead

### Command examples

```text
/ping\n
```

```text
/show\r\n
```

```text
/link\n
```

Expected replies are plain text lines such as:

- `pong`
- `link up`
- `link down`
- rendered config output

## Sending data to the Pico

To send bridged data over UART, write raw bytes.

The Pico forwards those bytes to the bridged TCP socket as-is.

Implications:

- binary payloads are supported
- the Pico does not append `\n` to payload traffic
- a leading `/` only becomes a local command if it appears at line start and stays printable through newline
- if you need strict application-level message boundaries, define them in your own payload protocol

### Data examples

If you write this to UART:

```text
hello from host\n
```

the Pico forwards:

```text
hello from host\n
```

If you write binary bytes such as:

```python
b'\x00\x01\x10\x1b\xf1\x99\xea\x88\xd33\x0c\x02\x00PY_UART_NODEWARNING: UART link check #2\x8c\xc1\x1f\xc5'
```

the Pico forwards the same bytes unchanged over TCP.

If you are using the helper terminal, it wraps chat lines with a sender label before writing them to UART, for example:

```text
[192.168.1.10] hello from host\n
```

That wrapping is done by the host tool, not by the firmware.

## Distinguishing commands from data

On UART, the split is based on line position and byte content:

- `/...<newline>` at line start with only printable ASCII -> local Pico command
- any other byte stream -> bridged data

Examples:

- `/ping\n` -> handled locally by the Pico
- `status please\n` -> forwarded to the remote peer
- `[host] hi\n` -> forwarded to the remote peer
- `b\"\\x00\\x01ABC\"` -> forwarded to the remote peer
- `b\"abc/def\"` -> forwarded to the remote peer because `/` was not at line start
- `b\"/bin\\x00\\n\"` -> forwarded as payload because the command candidate became non-printable

## Receiving data from the Pico

UART output from the Pico is quiet by default.

You may see:

- local command replies such as `pong`
- rendered config output when you explicitly request it
- application data received from the remote bridged peer

The firmware does not emit boot banners, prompts, or link-state chatter on UART unless you explicitly send a command that asks for information.

## Boot behavior

During boot, the firmware briefly exposes the configuration shell on UART before starting the network stack.

Current behavior:

- the shell has a fixed 3 second startup window
- incoming UART bytes no longer prevent Ethernet startup from beginning
- bytes received before the bridge session is up are not guaranteed to be forwarded

Once the network bridge is established, UART payload forwarding is binary-safe.

## Minimal examples

### Send a command with a serial terminal

```text
/ping
```

then press Enter.

### Send bridged data with a serial terminal

```text
hello from uart
```

then press Enter if you want the terminal to send a newline. The newline is part of your payload; it is not required by the bridge.

### Send from Python with pyserial

```python
import serial

ser = serial.Serial("/dev/ttyUSB0", 115200, timeout=1)
ser.write(b"/ping\n")
print(ser.readline())

ser.write(b"hello from host\n")
ser.write(b"\x00\x01\x10\x1b\xf1\x99")
```

## Host-side references

- Interactive UART terminal: [host/python/uart/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/link_terminal.py)
- Firmware UART bridge: [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- Boot/config shell: [src/shell.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/shell.rs)
