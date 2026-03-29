#!/usr/bin/env python3
"""I2C-based test tool for Pico-Fi communication."""

from __future__ import annotations

import argparse
import time

try:
    from .protocol import KIND_COMMAND, KIND_DATA, KIND_ERROR, read_packet, write_packet
    from .raw import open_bus
except ImportError:
    import os
    import sys

    sys.path.append(os.path.dirname(__file__))
    from protocol import KIND_COMMAND, KIND_DATA, KIND_ERROR, read_packet, write_packet
    from raw import open_bus

I2C_ADDR = 0x55
CHUNK_DELAY_S = 0.001
INITIAL_RESPONSE_WAIT_S = 0.01
COMMAND_TIMEOUT_S = 1.0


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:32])


def i2c_exchange(bus_num: int, payload: bytes, kind: int = KIND_DATA) -> int:
    bus = None
    try:
        bus = open_bus(bus_num)
        print(f"Sending to I2C addr 0x{I2C_ADDR:02x}...")
        print(f"Sent payload: {format_bytes(payload)}...")
        write_packet(bus, I2C_ADDR, payload, kind=kind, transfer_id=1, chunk_delay_s=CHUNK_DELAY_S)
        time.sleep(INITIAL_RESPONSE_WAIT_S)
        response = read_packet(
            bus,
            I2C_ADDR,
            chunk_delay_s=CHUNK_DELAY_S,
            timeout_s=COMMAND_TIMEOUT_S if kind == KIND_COMMAND else 0.1,
        )
        if response is None:
            print("Recv: <idle>")
            return 1
        rx_kind, body = response
        print(f"Recv kind: 0x{rx_kind:02x}")
        print(f"Recv payload: {format_bytes(body)}...")
        if body:
            try:
                print(f"Response: {body.decode('utf-8', errors='replace')!r}")
            except Exception:
                print(f"Response: {body.hex()}")
        return 0 if rx_kind in (KIND_DATA, KIND_COMMAND) else 1
    except Exception as exc:
        print(f"ERROR: I2C error - {exc}")
        return 1
    finally:
        if bus is not None:
            try:
                bus.close()
            except Exception:
                pass


def main() -> int:
    parser = argparse.ArgumentParser(description="I2C test tool for Pico-Fi")
    parser.add_argument("--bus", type=int, default=1, help="I2C bus number")
    parser.add_argument("--addr", type=int, default=0x55, help="Pico I2C address")
    subparsers = parser.add_subparsers(dest="command", required=True)
    probe_parser = subparsers.add_parser("probe", help="Probe with empty packets")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text", help="Command text (e.g., /ping)")
    args = parser.parse_args()
    global I2C_ADDR
    I2C_ADDR = args.addr
    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            print(f"\n--- Probe {i + 1} ---")
            if i2c_exchange(args.bus, b"") != 0:
                failures += 1
            time.sleep(0.2)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    if args.command == "command":
        return i2c_exchange(args.bus, (args.text + "\n").encode(), KIND_COMMAND)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
