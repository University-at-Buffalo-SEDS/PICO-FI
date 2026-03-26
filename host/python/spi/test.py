#!/usr/bin/env python3
"""SPI-based test tool for Pico-Fi communication."""

import argparse
import time

from .raw import FRAME_SIZE, open_bus

PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(2 + len(payload))
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    magic_val = frame[0]
    length = frame[1]
    if magic_val not in (RESP_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, 0, b""
    return magic_val, length, bytes(frame[2:2 + length])


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def spi_exchange(bus_num: int, device: int, speed: int, payload: bytes, magic: int = REQ_MAGIC) -> int:
    bus = None
    try:
        bus = open_bus(bus_num, device, speed)
        tx = build_frame(payload, magic)
        print(f"Sending to SPI bus {bus_num}.{device} @ {speed} Hz...")
        print(f"Sent: {format_bytes(tx)}...")
        bus.write_frame(tx)
        rx = bus.read_frame()
        magic_val, length, body = parse_frame(rx)
        if magic == REQ_COMMAND_MAGIC and magic_val != RESP_COMMAND_MAGIC:
            for _ in range(50):
                time.sleep(0.01)
                rx = bus.read_frame()
                magic_val, length, body = parse_frame(rx)
                if magic_val == RESP_COMMAND_MAGIC:
                    break
        print(f"Recv: {format_bytes(rx)}...")
        print(f"Magic: 0x{magic_val:02x}, Length: {length}")
        if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) and body:
            try:
                print(f"Response: {body.decode('utf-8', errors='replace')!r}")
            except Exception:
                print(f"Response: {body.hex()}")
        return 0 if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) else 1
    except Exception as exc:
        print(f"ERROR: SPI error - {exc}")
        return 1
    finally:
        if bus is not None:
            try:
                bus.close()
            except Exception:
                pass


def main() -> int:
    parser = argparse.ArgumentParser(description="SPI test tool for Pico-Fi")
    parser.add_argument("--bus", type=int, default=0, help="SPI bus number")
    parser.add_argument("--device", type=int, default=0, help="SPI device number")
    parser.add_argument("--speed", type=int, default=1_000_000, help="SPI speed in Hz")
    subparsers = parser.add_subparsers(dest="command", required=True)
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text", help="Command text (e.g., /ping)")
    args = parser.parse_args()
    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            print(f"\n--- Probe {i + 1} ---")
            if spi_exchange(args.bus, args.device, args.speed, b"") != 0:
                failures += 1
            time.sleep(0.2)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    if args.command == "command":
        return spi_exchange(
            args.bus,
            args.device,
            args.speed,
            (args.text + "\n").encode(),
            REQ_COMMAND_MAGIC,
        )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
