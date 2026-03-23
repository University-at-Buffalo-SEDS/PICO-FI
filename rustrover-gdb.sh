#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GDB_BIN="${GDB_BIN:-/opt/ST/STM32CubeCLT_1.20.0/GNU-tools-for-STM32/bin/arm-none-eabi-gdb}"
INIT_FILE="$ROOT_DIR/.gdbinit-rustrover"

exec "$GDB_BIN" -ix "$INIT_FILE" "$@"
