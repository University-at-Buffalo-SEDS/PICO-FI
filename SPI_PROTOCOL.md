# SPI Protocol

This project uses a custom framed SPI protocol between a Linux SPI master and the server Pico acting as an `SPI1` slave.

The Pi 5 is the SPI master. The server Pico is the SPI slave.

## Electrical setup

The server Pico uses `SPI1` on:

- `GPIO10` = `SCK`
- `GPIO11` = `MOSI` / slave RX
- `GPIO12` = `MISO` / slave TX
- `GPIO13` = `CSn`

Pi 5 wiring:

- Pi pin `23` `GPIO11 / SPI0_SCLK` -> Pico `GPIO10`
- Pi pin `19` `GPIO10 / SPI0_MOSI` -> Pico `GPIO11`
- Pi pin `21` `GPIO9 / SPI0_MISO` <- Pico `GPIO12`
- Pi pin `24` `GPIO8 / SPI0_CE0` -> Pico `GPIO13`
- Pi GND -> Pico GND

Recommended test settings:

- mode `0`
- `8` bits per word
- start at `50_000` Hz

## Transaction model

Each SPI transfer is a single fixed-size transaction of `258` bytes.

`CS` is the transaction boundary.

On `CS` low:

- the Pico starts a fresh slave transaction
- the Pico primes the response frame into the TX FIFO

On `CS` high:

- the Pico ends the transaction
- the Pico rearms for the next transaction

Do not treat this as a raw byte stream. One request is one full `258` byte transfer.

## Frame format

Request frame from the SPI master:

- byte `0`: request magic `0xA5`
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Response frame from the Pico:

- byte `0`: response magic `0x5A`
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Constants:

- frame size = `258`
- max payload = `256`

## Payload semantics

Payload is ASCII text.

For normal bridge operation, the payload should be line-oriented text ending with `\n`.

Examples:

- `hello from pi\n`
- `/ping\n`
- `/show\n`
- `/link\n`

Lines beginning with `/` are handled by the local Pico and are not forwarded across the Ethernet bridge.

Supported local Pico commands:

- `/help`
- `/show`
- `/ping`
- `/link`

Normal lines without `/` are forwarded as chat text across the Pico-to-Pico TCP bridge.

## Reference flow

To send a command:

1. Encode ASCII payload, usually including a trailing newline.
2. Build a `258` byte request frame.
3. Set byte `0` to `0xA5`.
4. Set byte `1` to payload length.
5. Copy payload into byte `2` onward.
6. Zero-fill the rest.
7. Perform one full-duplex `258` byte SPI transfer while asserting `CS`.
8. Validate that response byte `0` is `0x5A`.
9. Read response length from byte `1`.
10. Decode response bytes from `2` onward.

## Minimal pseudocode

```text
payload = b"/ping\n"
tx = [0] * 258
tx[0] = 0xA5
tx[1] = len(payload)
tx[2:2+len(payload)] = payload

rx = spi_transfer(tx)

assert rx[0] == 0x5A
length = rx[1]
body = rx[2:2+length]
```

## Host-side references

- Framed terminal: [spi_link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/spi_link_terminal.py)
- Test tool: [spi_test.py](/Users/rylan/Documents/GitKraken/pico-fi/spi_test.py)
- Firmware parser: [src/main.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/main.rs)

## Important note

Short ad hoc transfers like `16` byte probes are only health checks. They are not the real bridge protocol.

For actual application traffic, always send a full `258` byte framed request.
