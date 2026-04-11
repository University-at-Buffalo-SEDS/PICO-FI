Host-side Pico-Fi tooling

This directory contains the Linux and Python reference implementations for the Pico-Fi host transports.

## Layout

- `i2c_backend.rs`: Linux `/dev/i2c-*` backend for the Pico-Fi I2C transport
- `spi_backend.rs`: Linux `/dev/spidev*` backend for the Pico-Fi SPI transport
- `python/i2c/`: I2C tools and router
- `python/spi/`: SPI tools and router
- `python/uart/`: UART tools and router
- `python/telemetry_cli.py`: one-shot serialized telemetry send/recv helper for UART and SPI
- `python/telemetry_terminal.py`: interactive serialized telemetry terminal for UART and SPI
- `python/bridge_ssh_test.py`: end-to-end regression harness for the local-UART / remote-SPI test setup

## Current Transport Model

UART and SPI:

- use the same logical request/response framing
- frame size is `258` bytes
- request magics are `0xA5` for data and `0xA6` for commands
- response magics are `0x5A` for data and `0x5B` for command replies

I2C:

- uses `32` byte slots on the wire
- reassembles those slots into logical packets
- logical packet kinds are `KIND_DATA`, `KIND_COMMAND`, and `KIND_ERROR`

Protocol details are documented in:

- [UART_PROTOCOL.md](/Users/rylan/Documents/GitKraken/pico-fi/UART_PROTOCOL.md)
- [SPI_PROTOCOL.md](/Users/rylan/Documents/GitKraken/pico-fi/SPI_PROTOCOL.md)
- [I2C_PROTOCOL.md](/Users/rylan/Documents/GitKraken/pico-fi/I2C_PROTOCOL.md)

## Python Tools

Per backend:

- `test.py`: low-level transport helpers
- `link_terminal.py`: interactive plain-text bridge terminal
- `sedsprintf_router.py`: UDP-to-transport router using serialized `sedsprintf_rs_2026` packets

Current note:

- UART and SPI `test.py` expose `probe`, `command`, `send`, `recv`, and related helpers
- I2C `test.py` currently exposes `probe` and `command`

Telemetry validation:

- `telemetry_cli.py`: send or receive one serialized telemetry packet over UART or SPI
- `telemetry_terminal.py`: interactive terminal that sends each typed line as one serialized telemetry packet and prints decoded packets it receives

## Interactive Tooling

Plain bridge terminals:

```bash
python3 host/python/uart/link_terminal.py --port /dev/ttyUSB0 --baud 115200
python3 host/python/spi/link_terminal.py --bus 0 --device 0 --speed 100000
python3 host/python/i2c/link_terminal.py --bus 1 --addr 0x55
```

Telemetry terminals:

```bash
python3 host/python/telemetry_terminal.py --sender uart-node uart --port /dev/ttyUSB0 --speed 115200
python3 host/python/telemetry_terminal.py --sender spi-node spi --bus 0 --device 0 --speed 100000
```

Use the plain bridge terminals when you want to validate the transport itself with normal text payloads.

Use the telemetry terminal when you want to validate the firmware with the binary payload of a serialized telemetry packet.

## Example Commands

UART:

```bash
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 probe --count 3
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 command /ping
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 send "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 --speed 115200 recv --expect "hello"
```

SPI:

```bash
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 probe --count 3
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 command /link
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 send "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 recv --expect "hello"
```

I2C:

```bash
python3 host/python/i2c/test.py --bus 1 --addr 0x55 probe --count 3
python3 host/python/i2c/test.py --bus 1 --addr 0x55 command /ping
python3 host/python/i2c/test.py --bus 1 --addr 0x55 command /link
```

## Telemetry Helpers

One-shot helpers:

```bash
python3 host/python/telemetry_cli.py send --sender uart-node "hello" uart --port /dev/ttyUSB0 --speed 115200
python3 host/python/telemetry_cli.py recv --timeout 10 spi --bus 0 --device 0 --speed 100000
```

These helpers use the same transport adapters as the routers and terminals:

- UART telemetry rides inside normal `0xA5` UART data frames
- SPI telemetry rides inside normal `0xA5` SPI data frames
- the payload bytes are serialized `sedsprintf_rs_2026` packets

## sedsprintf Routers

The router scripts listen on local UDP, serialize those datagrams as `sedsprintf_rs_2026` packets, and send them over the selected Pico-Fi backend.

Examples:

```bash
python3 host/python/uart/sedsprintf_router.py --port /dev/ttyUSB0 --speed 115200 --listen-port 9000 --forward-port 9001 --sender uart-end
python3 host/python/spi/sedsprintf_router.py --bus 0 --device 0 --speed 100000 --listen-port 9000 --forward-port 9001 --sender spi-end
python3 host/python/i2c/sedsprintf_router.py --bus 1 --addr 0x55 --listen-port 9000 --forward-port 9001 --sender i2c-end
```

Paired UART/SPI example:

```bash
python3 host/python/uart/sedsprintf_router.py --port /dev/ttyUSB0 --speed 115200 --listen-port 9000 --forward-port 9001 --sender uart-end
python3 host/python/spi/sedsprintf_router.py --bus 0 --device 0 --speed 100000 --listen-port 9001 --forward-port 9000 --sender spi-end
```

## Automated Bridge Testing

The SSH harness validates the common local-UART / remote-SPI setup.

Example:

```bash
python3 host/python/bridge_ssh_test.py \
  --ssh-target rylan@10.8.0.6 \
  --remote-root /home/rylan/Documents/Git/PICO-FI \
  --uart-port /dev/cu.usbmodem21102 \
  --local-python ./venv/bin/python \
  --spi-speed 100000 \
  spi-probe
```

Useful subcommands:

- `spi-probe`
- `spi-command /link`
- `uart-to-spi --text hello`
- `spi-to-uart --text hello`
- `spi-link-terminal-soak --iterations 10`
- `spi-uart-soak --iterations 10 --check-commands`
- `telemetry-router-soak --iterations 10`
- `telemetry-once --direction uart-to-spi --text hello`

## Operational Notes

- Use only one process per UART device at a time.
- Use only one process per SPI device at a time.
- On macOS, prefer `/dev/cu.*` over `/dev/tty.*` for host-initiated UART traffic.
- Do not run automated tests while an interactive terminal holds the same device.

## Test Coverage

Current unit coverage includes:

- [host/python/test_sedsprintf_router_common.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/test_sedsprintf_router_common.py)
- [host/python/spi/test_sedsprintf_router.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/spi/test_sedsprintf_router.py)
- [host/python/test_telemetry_cli.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/test_telemetry_cli.py)

Run them with:

```bash
python3 -m unittest \
  host.python.test_sedsprintf_router_common \
  host.python.spi.test_sedsprintf_router \
  host.python.test_telemetry_cli
```
