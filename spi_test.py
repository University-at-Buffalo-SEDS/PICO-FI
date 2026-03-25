#!/usr/bin/env python3

from __future__ import annotations

import argparse
import sys
import time

try:
    import spidev
except ImportError as exc:  # pragma: no cover - runtime dependency
    raise SystemExit(
        "error: spidev is required. Install it with `python3 -m pip install spidev`."
    ) from exc


FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> list[int]:
    payload = payload[:PAYLOAD_MAX]
    frame = [0] * FRAME_SIZE
    frame[0] = magic
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return frame


def parse_frame(frame: list[int]) -> tuple[bool, int, bytes]:
    if len(frame) != FRAME_SIZE:
        return False, 0, b""
    if frame[0] not in (RESP_MAGIC, RESP_COMMAND_MAGIC):
        return False, frame[1] if len(frame) > 1 else 0, b""
    length = min(frame[1], PAYLOAD_MAX)
    return True, length, bytes(frame[2 : 2 + length])


def framed_probe(spi: spidev.SpiDev, count: int, delay_s: float) -> int:
    failures = 0
    for idx in range(count):
        tx = build_frame(b"", REQ_MAGIC)
        rx = spi.xfer2(tx)
        ok, length, body = parse_frame(rx)
        status = "ok" if ok else "bad"
        preview = format_bytes(rx[:16])
        if ok:
            print(
                f"probe {idx + 1:02d}: {status} rx0=0x{rx[0]:02x} len={length} preview={preview}"
            )
            if body:
                print(f"payload: {body.decode('utf-8', errors='replace')!r}")
        else:
            print(f"probe {idx + 1:02d}: {status} rx={preview}")
        if not ok:
            failures += 1
        if idx + 1 != count:
            time.sleep(delay_s)
    return failures


def framed_exchange(spi: spidev.SpiDev, payload: bytes, magic: int = REQ_MAGIC) -> int:
    tx = build_frame(payload, magic)
    orig = tx[:]
    rx = spi.xfer2(tx)
    ok, length, body = parse_frame(rx)
    print(f"frame tx: {format_bytes(orig[: min(24, len(orig))])} ...")
    print(f"frame rx: {format_bytes(rx[: min(24, len(rx))])} ...")
    print(f"valid response: {'yes' if ok else 'no'}")
    print(f"declared length: {length}")
    if body:
        print(f"payload: {body.decode('utf-8', errors='replace')!r}")
    else:
        print("payload: b''")
    return 0 if ok else 1


def encode_line_payload(text: str) -> bytes:
    data = text.encode("utf-8")
    if not data.endswith(b"\n"):
        data += b"\n"
    return data


def format_bytes(values: list[int]) -> str:
    return " ".join(f"{value:02x}" for value in values)


def main() -> int:
    parser = argparse.ArgumentParser(description="SPI test tool for the Pico SPI slave.")
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=50_000)
    parser.add_argument("--mode", type=int, default=0)

    subparsers = parser.add_subparsers(dest="command", required=True)

    probe_parser = subparsers.add_parser(
        "probe", help="Run repeated full-frame empty data requests."
    )
    probe_parser.add_argument("--count", type=int, default=10)
    probe_parser.add_argument("--delay-ms", type=int, default=100)

    frame_parser = subparsers.add_parser("frame", help="Send one full framed data request.")
    frame_parser.add_argument("payload", nargs="?", default="", help="ASCII payload to include in the frame.")

    line_parser = subparsers.add_parser("line", help="Send one full framed data line request ending in newline.")
    line_parser.add_argument("text", help="ASCII line to send, for example '/ping' or 'hello'.")

    command_parser = subparsers.add_parser("command", help="Send one full framed local-command request.")
    command_parser.add_argument("text", help="ASCII command to send, for example '/ping' or '/link'.")

    args = parser.parse_args()

    spi = spidev.SpiDev()
    spi.open(args.bus, args.device)
    spi.mode = args.mode
    spi.max_speed_hz = args.speed
    spi.bits_per_word = 8

    try:
        if args.command == "probe":
            return framed_probe(spi, args.count, args.delay_ms / 1000.0)
        if args.command == "frame":
            return framed_exchange(spi, args.payload.encode("utf-8"))
        if args.command == "line":
            return framed_exchange(spi, encode_line_payload(args.text))
        if args.command == "command":
            return framed_exchange(spi, encode_line_payload(args.text), REQ_COMMAND_MAGIC)
        parser.error("unknown command")
    finally:
        spi.close()

    return 1


if __name__ == "__main__":
    raise SystemExit(main())
