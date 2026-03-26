#!/usr/bin/env python3
"""I2C-based test tool for Pico-Fi communication."""

import argparse
import time

try:
    from .raw import CHUNK_SIZE, open_bus
except ImportError:
    import os
    import sys

    sys.path.append(os.path.dirname(__file__))
    from raw import CHUNK_SIZE, open_bus

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
I2C_ADDR = 0x55
CHUNK_DELAY_S = 0.001
INITIAL_RESPONSE_WAIT_S = 0.01
COMMAND_POLL_DELAY_S = 0.01
COMMAND_TIMEOUT_POLLS = 40

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


def is_garbage_frame(frame: bytes) -> bool:
    ff_count = sum(1 for b in frame if b == 0xFF)
    return ff_count > len(frame) * 0.8


def parse_frame(frame: bytes) -> tuple[int, int, bytes]:
    if len(frame) != FRAME_SIZE or is_garbage_frame(frame):
        return 0, 0, b""
    magic_val = frame[0]
    length = frame[1]
    if magic_val not in (RESP_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, 0, b""
    return magic_val, length, bytes(frame[2:2 + length])


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def read_frame(bus) -> bytes:
    rx = bytearray()
    for _ in range(0, FRAME_SIZE, CHUNK_SIZE):
        chunk_size = min(CHUNK_SIZE, FRAME_SIZE - len(rx))
        chunk = bus.read(I2C_ADDR, chunk_size)
        rx.extend(chunk)
        if len(rx) < FRAME_SIZE:
            time.sleep(CHUNK_DELAY_S)
    return bytes(rx[:FRAME_SIZE])


def write_frame(bus, frame: bytes) -> None:
    for i in range(0, len(frame), CHUNK_SIZE):
        chunk = frame[i:i + CHUNK_SIZE]
        bus.write(I2C_ADDR, chunk)
        if i + CHUNK_SIZE < len(frame):
            time.sleep(CHUNK_DELAY_S)


def i2c_exchange(bus_num: int, payload: bytes, magic: int = REQ_MAGIC) -> int:
    bus = None
    try:
        bus = open_bus(bus_num)
        tx = build_frame(payload, magic)
        print(f"Sending to I2C addr 0x{I2C_ADDR:02x}...")
        print(f"Sent: {format_bytes(tx)}...")
        write_frame(bus, tx)
        time.sleep(INITIAL_RESPONSE_WAIT_S)
        rx = read_frame(bus)
        magic_val, length, body = parse_frame(rx)
        if magic == REQ_COMMAND_MAGIC and magic_val != RESP_COMMAND_MAGIC:
            for _ in range(COMMAND_TIMEOUT_POLLS):
                time.sleep(COMMAND_POLL_DELAY_S)
                rx = read_frame(bus)
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
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
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
        return i2c_exchange(args.bus, (args.text + "\n").encode(), REQ_COMMAND_MAGIC)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
