# UART Protocol

This document describes how to talk to the Pico over its UART upstream interface.

Unlike SPI, UART is line-oriented. The Pico buffers incoming ASCII until newline and then decides whether the line is:

- a local Pico command, or
- bridged data that should be forwarded over Ethernet

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

UART input is interpreted as lines.

Rules:

- `\r` is ignored
- `\n` ends the current line
- ASCII bytes are appended to the current line buffer
- non-ASCII bytes are ignored by the bridge handler

The Pico does not act on a UART line until newline is received.

## Sending commands to the Pico

To issue a local Pico command over UART:

1. Send an ASCII line beginning with `/`.
2. Terminate the line with `\n` or `\r\n`.

Supported local commands:

- `/help`
- `/show`
- `/ping`
- `/link`

These are handled locally on the Pico and are not forwarded over the Ethernet bridge.

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

To send bridged data over UART:

1. Send an ASCII line that does not begin with `/`.
2. Terminate it with `\n` or `\r\n`.

The Pico forwards the line bytes to the bridged TCP socket and appends `\n`.

### Important limitation

The UART bridge is text-oriented, not binary-safe.

Implications:

- raw binary payloads are not suitable over UART
- non-ASCII bytes are dropped
- newline ends the message
- a line that begins with `/` is treated as a local Pico command

If you need to send arbitrary binary packets, use the SPI transport instead.

### Data examples

If you write this to UART:

```text
hello from host\n
```

the Pico forwards:

```text
hello from host\n
```

If you are using the helper terminal, it wraps chat lines with a sender label before writing them to UART, for example:

```text
[192.168.1.10] hello from host\n
```

That wrapping is done by the host tool, not by the firmware.

## Distinguishing command lines from data lines

On UART, the split is content-based:

- line starts with `/` -> local Pico command
- any other line -> bridged data

Examples:

- `/ping\n` -> handled locally by the Pico
- `status please\n` -> forwarded to the remote peer
- `[host] hi\n` -> forwarded to the remote peer

## Receiving data from the Pico

UART output from the Pico is plain text.

You may see:

- boot banner and configuration-shell prompts before the network starts
- local command replies such as `pong`
- bridge status lines such as `connecting`, `tcp connected`, `link active`
- application data received from the remote bridged peer

Because all of these share the same UART output, a host application should treat UART RX as human-readable text, not as a framed binary protocol.

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

then press Enter.

### Send from Python with pyserial

```python
import serial

ser = serial.Serial("/dev/ttyUSB0", 115200, timeout=1)
ser.write(b"/ping\n")
print(ser.readline())

ser.write(b"hello from host\n")
```

## Host-side references

- Interactive UART terminal: [uart_link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/uart_link_terminal.py)
- Firmware UART bridge: [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- Boot/config shell: [src/shell.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/shell.rs)
