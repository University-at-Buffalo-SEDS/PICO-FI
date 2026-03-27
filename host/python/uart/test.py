#!/usr/bin/env python3
"""UART-based test tool for Pico-Fi communication."""

import argparse
import time

import serial


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def read_line(ser: serial.Serial, timeout_s: float) -> bytes:
    deadline = time.monotonic() + timeout_s
    buf = bytearray()
    while time.monotonic() < deadline:
        chunk = ser.read(256)
        if not chunk:
            continue
        buf.extend(chunk)
        if b"\n" in buf:
            line, _, _rest = buf.partition(b"\n")
            return line.rstrip(b"\r")
    return bytes(buf)


def uart_exchange(port: str, speed: int, payload: str) -> int:
    try:
        with serial.Serial(port, speed, timeout=0.1) as ser:
            ser.reset_input_buffer()
            tx = payload.encode("utf-8")
            ser.write(tx)
            ser.flush()
            print(f"Sent: {format_bytes(tx)}...")
            rx = read_line(ser, 2.0)
            if not rx:
                print("ERROR: No UART response")
                return 1
            print(f"Recv: {format_bytes(rx)}...")
            print(f"Response: {rx.decode('utf-8', errors='replace')!r}")
            return 0
    except serial.SerialException as exc:
        print(f"ERROR: Serial error - {exc}")
        print(f"Make sure device is connected to {port}")
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description="UART test tool for Pico-Fi")
    parser.add_argument("--port", default="/dev/ttyAMA0")
    parser.add_argument("--speed", type=int, default=115200)
    subparsers = parser.add_subparsers(dest="command", required=True)
    probe_parser = subparsers.add_parser("probe", help="Probe with /ping")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text")
    args = parser.parse_args()
    if args.command == "probe":
        failures = 0
        for _ in range(args.count):
            if uart_exchange(args.port, args.speed, "/ping\n") != 0:
                failures += 1
            time.sleep(0.1)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    if args.command == "command":
        return uart_exchange(args.port, args.speed, args.text + "\n")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
