#!/usr/bin/env bash

set -euo pipefail

find_openocd() {
  if [[ -n "${OPENOCD_BIN:-}" ]]; then
    printf '%s\n' "$OPENOCD_BIN"
    return 0
  fi

  local candidate
  for candidate in /opt/homebrew/bin/openocd /usr/local/bin/openocd; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  if command -v openocd >/dev/null 2>&1; then
    command -v openocd
    return 0
  fi

  printf 'error: openocd not found. Install it or set OPENOCD_BIN.\n' >&2
  return 1
}

OPENOCD_BIN="$(find_openocd)"
OPENOCD_INTERFACE="${OPENOCD_INTERFACE:-interface/cmsis-dap.cfg}"
OPENOCD_TARGET="${OPENOCD_TARGET:-target/rp2040.cfg}"
OPENOCD_SPEED="${OPENOCD_SPEED:-2000}"
OPENOCD_GDB_PORT="${OPENOCD_GDB_PORT:-3333}"
OPENOCD_TELNET_PORT="${OPENOCD_TELNET_PORT:-disabled}"
OPENOCD_TCL_PORT="${OPENOCD_TCL_PORT:-disabled}"

printf 'OpenOCD gdb server: localhost:%s\n' "$OPENOCD_GDB_PORT"
printf 'Interface config: %s\n' "$OPENOCD_INTERFACE"
printf 'Target config: %s\n' "$OPENOCD_TARGET"

exec "$OPENOCD_BIN" \
  -f "$OPENOCD_INTERFACE" \
  -f "$OPENOCD_TARGET" \
  -c "adapter speed $OPENOCD_SPEED" \
  -c "gdb_port $OPENOCD_GDB_PORT" \
  -c "telnet_port $OPENOCD_TELNET_PORT" \
  -c "tcl_port $OPENOCD_TCL_PORT" \
  -c "rp2040.core0 configure -event gdb-attach { reset halt }"
