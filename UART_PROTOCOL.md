# UART Protocol

This project uses `UART0` as a framed upstream transport after the short boot-time shell exits.

The runtime UART wire format now matches the same fixed-size request/response frame layout used by the SPI transport.

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

- adapter `TX` -> Pico `GPIO1` (`RX`)
- adapter `RX` -> Pico `GPIO0` (`TX`)
- adapter `GND` -> Pico `GND`

## Frame Layout

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

## Semantics

- `0xA5` requests carry raw bridged data.
- `0xA6` requests carry local Pico commands such as `/ping`, `/show`, and `/link`.
- `0x5A` responses carry raw bridged data or an empty probe/idle reply.
- `0x5B` responses carry local Pico command replies or transport errors.

## Boot Behavior

During early boot, UART is temporarily attached to the configuration shell for about `3000 ms`.

During that window the Pico can emit ASCII lines such as:

- `pico-fi uart bridge`
- `booting with compiled config`
- configuration help text

After startup/config finishes, UART switches to framed binary packets.

Implication for host drivers:

- if the Pico has just reset, either wait at least `3` seconds before starting framed UART traffic
- or explicitly send `start\r\n` during the shell window and wait for the bridge to enter runtime mode

## Current Status

- Empty UART probes are currently reliable.
- Framed UART command handling is now implemented, but if you are observing failures you should verify that:
  - the Pico is actually running `upstream=uart`
  - you are talking to the runtime UART path, not only the boot shell
  - you are sending full `258` byte frames, not raw text lines

## Host Requirements

A correct host driver should:

- open the serial device in raw mode
- configure `115200 8N1`
- disable canonical mode, echo, software flow control, and hardware flow control
- send and receive full `258` byte frames
- treat payload bytes as binary, not C strings

The host should not:

- append newline framing in runtime mode
- stop reading at `0x00`
- assume partial reads are complete frames

## Recommended Examples

Use these in order:

1. Probe baseline:
   `python3 -m host.python.uart.test probe --port /dev/ttyUSB0 --count 10`
2. Command test:
   `python3 -m host.python.uart.test command /ping --port /dev/ttyUSB0`
3. Interactive terminal:
   `python3 -m host.python.uart.link_terminal --port /dev/ttyUSB0`

If you are using a Raspberry Pi Debug Probe or another debugger that exposes the Pico UART as a USB modem device, substitute that device path instead of `/dev/ttyUSB0`.
