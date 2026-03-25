#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INIT_FILE="$ROOT_DIR/.gdbinit-rustrover"

find_gdb() {
  if [[ -n "${GDB_BIN:-}" ]]; then
    printf '%s\n' "$GDB_BIN"
    return 0
  fi

  local candidate
  for candidate in \
    /opt/homebrew/bin/arm-none-eabi-gdb \
    /usr/local/bin/arm-none-eabi-gdb \
    /opt/ST/STM32CubeCLT_1.20.0/GNU-tools-for-STM32/bin/arm-none-eabi-gdb
  do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  if command -v arm-none-eabi-gdb >/dev/null 2>&1; then
    command -v arm-none-eabi-gdb
    return 0
  fi

  printf 'error: arm-none-eabi-gdb not found. Install it or set GDB_BIN.\n' >&2
  return 1
}

GDB_BIN="$(find_gdb)"

exec "$GDB_BIN" -ix "$INIT_FILE" "$@"
