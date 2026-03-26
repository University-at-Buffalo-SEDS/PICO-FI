# I2C Protocol

This project uses a framed I2C protocol between a Linux I2C master and the Pico acting as an `I2C0` slave at address `0x55`.

## Electrical setup

The Pico uses:

- `GPIO0` = `I2C0 SDA`
- `GPIO1` = `I2C0 SCL`

Pi wiring:

- Pi SDA -> Pico `GPIO0`
- Pi SCL -> Pico `GPIO1`
- Pi GND -> Pico GND

## Frame layout

Each request and response is a fixed `258` byte frame:

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
- `0xA6` requests carry local Pico commands such as `/ping` and `/show`.
- Empty `0xA5` requests are polling requests used to fetch pending bridge data.

## References

- Interactive I2C terminal: [host/python/i2c/link_terminal.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/link_terminal.py)
- I2C test tool: [host/python/i2c/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/i2c/test.py)
- Firmware I2C bridge: [src/bridge/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/i2c.rs)
- I2C protocol constants: [src/protocol/i2c.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/protocol/i2c.rs)
