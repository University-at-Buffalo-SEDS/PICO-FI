#!/usr/bin/env python3
"""UART-based test tool for Pico-Fi communication."""

import argparse
import time

import serial

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2 + len(payload)] = payload
    return bytes(frame)


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def uart_exchange(port: str, speed: int, payload: bytes, magic: int = REQ_MAGIC) -> int:
    try:
        with serial.Serial(port, speed, timeout=2) as ser:
            tx = build_frame(payload, magic)
            ser.write(tx)
            print(f"Sent: {format_bytes(tx)}...")
            rx = ser.read(FRAME_SIZE)
            if len(rx) < FRAME_SIZE:
                print(f"ERROR: Incomplete response ({len(rx)} bytes)")
                return 1
            print(f"Recv: {format_bytes(bytes(rx))}...")
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
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text")
    args = parser.parse_args()
    if args.command == "probe":
        failures = 0
        for _ in range(args.count):
            if uart_exchange(args.port, args.speed, b"") != 0:
                failures += 1
            time.sleep(0.1)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    if args.command == "command":
        return uart_exchange(args.port, args.speed, (args.text + "\n").encode(), REQ_COMMAND_MAGIC)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
