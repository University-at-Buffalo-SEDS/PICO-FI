# SPI Protocol

This project uses a framed SPI protocol between a Linux SPI master and the Pico acting as an `SPI1` slave.

The Pi 5 is the SPI master. The Pico is the SPI slave.

## Electrical setup

The Pico uses `SPI1` on:

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

Recommended starting settings:

- mode `0`
- `8` bits per word
- start at `50_000` Hz

## Transaction model

Each SPI request is one fixed-size `258` byte transfer.

`CS` is the transaction boundary.

On `CS` low:

- the Pico starts a fresh slave transaction
- the Pico primes its response bytes into the TX FIFO

On `CS` high:

- the Pico ends the transaction
- the Pico rearms for the next request

Do not treat SPI as a raw stream. One logical request is one full `258` byte transaction.

## Frame layout

Request frame from the SPI master:

- byte `0`: request magic
- byte `1`: payload length `N`
- bytes `2..(2+N)`: payload
- remaining bytes: zero padding

Response frame from the Pico:

- byte `0`: response magic
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

## Two request types

There are two different request classes. This is the important split:

- `0xA5` means bridge data
- `0xA6` means local Pico command

The firmware decides what to do from the frame magic, not from the payload contents.

That means binary payload bytes, leading `/`, newline bytes, or arbitrary packet headers inside a `0xA5` data frame do not trigger the local command handler.

## Sending data to the Pico

Use `0xA5` when you want the Pico to forward payload bytes across the Ethernet bridge.

The payload is treated as raw data:

- no ASCII requirement
- no line parsing
- no `/command` detection
- no automatic newline insertion

If your application usually sends binary packets, put the packet bytes directly in the `0xA5` payload.

Example binary payload shape:

```text
[FLAGS: u8]
[NEP: u8]
VARINT(ty)
VARINT(data_size)
VARINT(timestamp_ms)
VARINT(sender_len)
[VARINT(sender_wire_len)]
ENDPOINTS_BITMAP
SENDER BYTES
[RELIABLE HEADER]
PAYLOAD BYTES
[CRC32: u32 LE]
```

That whole byte sequence can be copied into the SPI data-frame payload unchanged.

### Data frame recipe

1. Build a `258` byte request buffer.
2. Set byte `0` to `0xA5`.
3. Set byte `1` to payload length.
4. Copy your raw payload bytes into byte `2`.
5. Zero-fill the rest of the frame.
6. Perform one full-duplex `258` byte SPI transfer while `CS` is asserted.
7. Validate that response byte `0` is `0x5A` or `0x5B`.
8. Read response length from byte `1`.
9. Read response data from bytes `2..(2+N)`.

### Data frame example

```text
payload = binary_packet
tx = [0] * 258
tx[0] = 0xA5
tx[1] = len(payload)
tx[2:2+len(payload)] = payload

rx = spi_transfer(tx)
assert rx[0] in (0x5A, 0x5B)
```

## Sending commands to the Pico

Use `0xA6` when you want the Pico itself to handle a local command.

Command payloads are ASCII text. In practice you should send a trailing newline.

Supported local commands:

- `/help`
- `/show`
- `/ping`
- `/link`

These commands are handled locally on the Pico and are not forwarded over the Ethernet bridge.

### Command frame recipe

1. Encode the command as ASCII bytes, usually with trailing `\n`.
2. Build a `258` byte request buffer.
3. Set byte `0` to `0xA6`.
4. Set byte `1` to payload length.
5. Copy the command bytes into byte `2`.
6. Zero-fill the rest of the frame.
7. Perform one full-duplex `258` byte SPI transfer while `CS` is asserted.
8. Validate that response byte `0` is `0x5A` or `0x5B`.
9. Read response length from byte `1`.
10. Decode bytes `2..(2+N)` as the Pico's reply.

### Command examples

```text
payload = b"/ping\n"
tx = [0] * 258
tx[0] = 0xA6
tx[1] = len(payload)
tx[2:2+len(payload)] = payload

rx = spi_transfer(tx)
assert rx[0] == 0x5B
length = rx[1]
body = rx[2:2+length]
```

```text
payload = b"/show\n"
```

```text
payload = b"/link\n"
```

## Response semantics

The Pico can return either:

- `0x5A`: data payload waiting from the bridged Ethernet side
- `0x5B`: direct reply to a local Pico command

In polling setups it is normal to issue empty data requests to pull waiting bridge data from the Pico.

Example empty poll:

```text
tx = [0] * 258
tx[0] = 0xA5
tx[1] = 0
rx = spi_transfer(tx)
```

## Host-side references

- Interactive SPI terminal: [spi_link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/spi_link_terminal.py)
- SPI test tool: [spi_test.py](/Users/rylan/Documents/GitKraken/pico-fi/spi_test.py)
- Firmware SPI bridge: [src/bridge/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/spi.rs)
- SPI protocol constants: [src/protocol/spi.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/spi.rs)

## Important note

Short ad hoc transfers like `16` byte probes are only health checks. They are not the real bridge protocol.

For actual traffic, always send a full `258` byte framed request.
