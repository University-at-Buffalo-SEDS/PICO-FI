#!/bin/sh
# Persistent, matched transport profile; never flashes hardware.
set -eu
cd "$(dirname "$0")/.."
PICO_FI_CONFIG=pico-fi-server.json PICO_FI_UART_BAUD_RATE=1000000 cargo build --release
cp target/thumbv6m-none-eabi/release/pico-fi target/pico-fi-groundstation-i2c.elf
PICO_FI_CONFIG=pico-fi-client.json PICO_FI_UART_BAUD_RATE=1000000 cargo build --release
cp target/thumbv6m-none-eabi/release/pico-fi target/pico-fi-gateway-uart-1mbaud.elf
