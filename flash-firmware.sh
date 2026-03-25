#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ELF_PATH="${1:-$ROOT_DIR/target/thumbv6m-none-eabi/debug/pico-fi}"
PROBE_RS_BIN="${PROBE_RS_BIN:-}"
PROBE_RS_CHIP="${PROBE_RS_CHIP:-RP2040}"
PROBE_RS_PROBE="${PROBE_RS_PROBE:-}"

find_probe_rs() {
  if [[ -n "$PROBE_RS_BIN" ]]; then
    printf '%s\n' "$PROBE_RS_BIN"
    return 0
  fi

  local candidate
  for candidate in /Users/rylan/.cargo/bin/probe-rs /opt/homebrew/bin/probe-rs /usr/local/bin/probe-rs; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  if command -v probe-rs >/dev/null 2>&1; then
    command -v probe-rs
    return 0
  fi

  printf 'error: probe-rs not found. Install it or set PROBE_RS_BIN.\n' >&2
  return 1
}

if [[ ! -f "$ELF_PATH" ]]; then
  printf 'error: ELF not found: %s\n' "$ELF_PATH" >&2
  exit 1
fi

PROBE_RS_BIN="$(find_probe_rs)"

download_cmd=("$PROBE_RS_BIN" download "$ELF_PATH" --chip "$PROBE_RS_CHIP")
reset_cmd=("$PROBE_RS_BIN" reset --chip "$PROBE_RS_CHIP")

if [[ -n "$PROBE_RS_PROBE" ]]; then
  download_cmd+=(--probe "$PROBE_RS_PROBE")
  reset_cmd+=(--probe "$PROBE_RS_PROBE")
fi

"${download_cmd[@]}"
"${reset_cmd[@]}"
