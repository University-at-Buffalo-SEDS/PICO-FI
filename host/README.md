Host-side Pico-Fi tooling

## Layout

- `i2c_backend.rs`: Linux `/dev/i2c-*` backend for the Pico-Fi I2C transport
- `spi_backend.rs`: Linux `/dev/spidev*` backend for the Pico-Fi SPI transport
- `python/i2c/`: I2C tools and router
- `python/spi/`: SPI tools and router
- `python/uart/`: UART tools and router
- `python/bridge_ssh_test.py`: end-to-end regression harness for the Pi-connected SPI server and local UART client

## Python Tools

Each backend has:

- `test.py`: probe, command, send, and recv helpers
- `link_terminal.py`: interactive terminal
- `sedsprintf_router.py`: UDP-to-Fi router using `sedsprintf_rs_2026`

Interactive terminal behavior is now consistent across UART, I2C, and SPI:

- plain text sends bridged data
- `/...` sends a local Pico command
- outbound chat is rendered as `sender: message`

## Backend Notes

UART:

- framed binary runtime, `115200 8N1`
- boot shell is still present briefly after reset
- use `host/python/uart/test.py` and `host/python/uart/link_terminal.py`

I2C:

- Linux host uses 32-byte slot framing on the wire
- logical message semantics still map to data and command payloads
- use `host/python/i2c/test.py` and `host/python/i2c/link_terminal.py`

SPI:

- Linux host must use mode `3`
- all traffic is sent as fixed 258-byte full-duplex transactions
- empty probe/poll uses `0xA5`
- non-empty host traffic uses the stable `0xA6` path
- only one SPI client should access `/dev/spidev*` at a time

## Example Commands

UART:

```bash
python3 host/python/uart/test.py --port /dev/ttyUSB0 probe --count 3
python3 host/python/uart/test.py --port /dev/ttyUSB0 command /ping
python3 host/python/uart/test.py --port /dev/ttyUSB0 send "hello"
python3 host/python/uart/test.py --port /dev/ttyUSB0 recv --expect "hello"
python3 host/python/uart/link_terminal.py --port /dev/ttyUSB0
```

I2C:

```bash
python3 host/python/i2c/test.py --bus 1 probe --count 3
python3 host/python/i2c/test.py --bus 1 command /ping
python3 host/python/i2c/test.py --bus 1 command /link
python3 host/python/i2c/link_terminal.py --bus 1 --addr 0x55
```

SPI:

```bash
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 probe --count 3
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 command /link
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 send "hello"
python3 host/python/spi/test.py --bus 0 --device 0 --speed 100000 recv --expect "hello"
python3 host/python/spi/link_terminal.py --bus 0 --device 0 --speed 100000
```

## sedsprintf Routers

The router scripts listen on a local UDP port, wrap datagrams in `sedsprintf_rs_2026` packets, carry them over the chosen Fi backend, and forward received payloads to another local UDP port.

Examples:

```bash
python3 host/python/uart/sedsprintf_router.py --port /dev/ttyUSB0 --listen-port 9000 --forward-port 9001 --sender uart-end
python3 host/python/i2c/sedsprintf_router.py --bus 1 --addr 0x55 --listen-port 9000 --forward-port 9001 --sender i2c-end
python3 host/python/spi/sedsprintf_router.py --bus 0 --device 0 --speed 100000 --listen-port 9000 --forward-port 9001 --sender spi-end
```

Paired router examples:

```bash
python3 host/python/uart/sedsprintf_router.py --port /dev/ttyUSB0 --listen-port 9000 --forward-port 9001 --sender uart-end
python3 host/python/spi/sedsprintf_router.py --bus 0 --device 0 --speed 100000 --listen-port 9001 --forward-port 9000 --sender spi-end
```

```bash
python3 host/python/i2c/sedsprintf_router.py --bus 1 --addr 0x55 --listen-port 9000 --forward-port 9001 --sender i2c-end
python3 host/python/uart/sedsprintf_router.py --port /dev/ttyUSB0 --listen-port 9001 --forward-port 9000 --sender uart-end
```

## Automated Bridge Testing

Use the SSH harness to validate the local UART client Pico against the remote SPI server Pico:

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

Do not run automated tests while an interactive terminal is holding the same SPI or UART device.
