#!/usr/bin/env python3
"""UART framed test tool for Pico-Fi communication."""

from __future__ import annotations

import argparse

import serial
import sys
import time

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_DATA_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B
DATA_POLL_LIMIT = 50


def format_bytes(data: bytes, limit: int = 16) -> str:
    return " ".join(f"{b:02x}" for b in data[:limit])


def open_serial(port: str, speed: int) -> serial.Serial:
    return serial.Serial(
        port=port,
        baudrate=speed,
        timeout=0.1,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_ONE,
        xonxoff=False,
        rtscts=False,
        dsrdtr=False,
    )


def build_frame(payload: bytes, magic: int) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2: 2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    magic = frame[0]
    length = frame[1]
    if magic not in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, 0, b""
    payload = bytes(frame[2: 2 + length])
    return magic, length, payload


def read_frame(ser: serial.Serial, timeout_s: float) -> bytes:
    deadline = time.monotonic() + timeout_s
    buf = bytearray()
    while time.monotonic() < deadline and len(buf) < FRAME_SIZE:
        chunk = ser.read(FRAME_SIZE - len(buf))
        if chunk:
            buf.extend(chunk)
    return bytes(buf)


def uart_exchange(
        port: str,
        speed: int,
        payload: bytes,
        magic: int,
        await_nonempty_data: bool = False,
        expect_valid_response: bool = True,
        expected_text: str | None = None,
) -> int:
    try:
        with open_serial(port, speed) as ser:
            ser.reset_input_buffer()
            tx = build_frame(payload, magic)
            ser.write(tx)
            ser.flush()
            print(f"Sent: {format_bytes(tx)}...")

            rx = read_frame(ser, 2.0)
            if len(rx) != FRAME_SIZE:
                if await_nonempty_data and magic == REQ_DATA_MAGIC and not payload:
                    for _ in range(DATA_POLL_LIMIT):
                        time.sleep(0.05)
                        ser.write(tx)
                        ser.flush()
                        rx = read_frame(ser, 0.5)
                        if len(rx) == FRAME_SIZE:
                            break
                if len(rx) != FRAME_SIZE:
                    print("ERROR: No UART frame response")
                    return 1

            magic_val, length, body = parse_frame(rx)
            if await_nonempty_data and magic_val == RESP_DATA_MAGIC and not body:
                for _ in range(DATA_POLL_LIMIT):
                    time.sleep(0.05)
                    ser.write(tx)
                    ser.flush()
                    rx = read_frame(ser, 0.5)
                    if len(rx) != FRAME_SIZE:
                        continue
                    magic_val, length, body = parse_frame(rx)
                    if magic_val == RESP_DATA_MAGIC and body:
                        break
            print(f"Recv: {format_bytes(rx)}...")
            print(f"Magic: 0x{magic_val:02x}, Length: {length}")
            if body:
                print(f"Response: {body.decode('utf-8', errors='replace')!r}")
            if expected_text is not None:
                actual = body.decode("utf-8", errors="replace")
                if expected_text not in actual:
                    print(
                        f"ERROR: Expected response containing {expected_text!r}, got {actual!r}",
                        file=sys.stderr,
                    )
                    return 1
            if expect_valid_response:
                return 0 if magic_val in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) else 1
            return 0
    except serial.SerialException as exc:
        print(f"ERROR: Serial error - {exc}")
        print(f"Make sure device is connected to {port}")
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description="UART framed test tool for Pico-Fi")
    parser.add_argument("--port", default="/dev/ttyAMA0")
    parser.add_argument("--speed", type=int, default=115200)
    subparsers = parser.add_subparsers(dest="command", required=True)
    probe_parser = subparsers.add_parser("probe", help="Probe with empty framed data packets")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send framed command")
    cmd_parser.add_argument("text")
    data_parser = subparsers.add_parser("data", help="Send framed data and optionally await a non-empty data reply")
    data_parser.add_argument("text")
    data_parser.add_argument(
        "--expect",
        default="",
        help="Substring expected in the returned data payload.",
    )
    send_parser = subparsers.add_parser("send", help="Send framed data and only require a valid immediate response")
    send_parser.add_argument("text")
    recv_parser = subparsers.add_parser("recv",
                                        help="Poll with empty framed data packets until non-empty data is returned")
    recv_parser.add_argument(
        "--expect",
        default="",
        help="Substring expected in the returned data payload.",
    )
    args = parser.parse_args()

    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            print(f"\n--- Probe {i + 1} ---")
            if uart_exchange(args.port, args.speed, b"", REQ_DATA_MAGIC) != 0:
                failures += 1
            time.sleep(0.05)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1

    if args.command == "command":
        return uart_exchange(
            args.port,
            args.speed,
            (args.text + "\n").encode(),
            REQ_COMMAND_MAGIC,
        )
    if args.command == "data":
        return uart_exchange(
            args.port,
            args.speed,
            args.text.encode(),
            REQ_DATA_MAGIC,
            await_nonempty_data=True,
            expected_text=args.expect or None,
        )
    if args.command == "send":
        return uart_exchange(
            args.port,
            args.speed,
            args.text.encode(),
            REQ_DATA_MAGIC,
        )
    if args.command == "recv":
        return uart_exchange(
            args.port,
            args.speed,
            b"",
            REQ_DATA_MAGIC,
            await_nonempty_data=True,
            expected_text=args.expect or None,
        )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
