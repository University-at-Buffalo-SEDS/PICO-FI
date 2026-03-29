# UART Protocol

This project now uses UART as a raw binary upstream transport after boot.

## Physical link

Default UART settings:

- `UART0`
- `115200` baud
- `8N1`

Pins:

- `GPIO0` = UART TX from Pico
- `GPIO1` = UART RX into Pico

## Runtime behavior

Once the bridge is running, every byte received on UART is forwarded across the bridged TCP link immediately.

Rules:

- no ASCII command mode
- no slash-command parsing
- no newline framing
- binary bytes are forwarded unchanged
- data received from the remote peer is written back to UART unchanged

If the UART egress side falls behind, the firmware uses a lossy overwrite ring and drops the oldest buffered outbound UART bytes instead of stalling the bridge.

## Boot behavior

During early boot, the configuration shell still temporarily uses UART before bridge mode starts.

After that startup/config window ends and UART bridge mode begins, UART is pure binary relay mode.

## References

- Firmware UART bridge: [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- Boot/config shell: [src/shell.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/shell.rs)
