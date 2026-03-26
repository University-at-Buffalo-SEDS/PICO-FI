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
RESP_DATA_MAGIC = 0x5A  # Alias for data responses
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> list[int]:
    payload = payload[:PAYLOAD_MAX]
    frame = [0] * FRAME_SIZE
    frame[0] = magic
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return frame


def parse_frame(frame: list[int]) -> tuple[int, int, bytes]:
    """Parse a single-byte response from SPI transaction (due to 1-byte FIFO limit)"""
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    
    # With 1-byte FIFO limit, each transaction returns 1 byte of the response
    # Extract the first non-zero byte (or first byte if all zeros)
    for byte_val in frame[:10]:  # Check first 10 bytes
        if byte_val != 0:
            return 0, 1, bytes([byte_val])
    
    # All zeros - might be padding
    return 0, 0, b""


def framed_probe(spi: spidev.SpiDev, count: int, delay_s: float) -> int:
    failures = 0
    for idx in range(count):
        tx = build_frame(b"", REQ_MAGIC)
        rx = spi.xfer2(tx)
        resp_magic, length, body = parse_frame(rx)
        ok = resp_magic in (RESP_MAGIC, RESP_COMMAND_MAGIC)
        status = "ok" if ok else "bad"
        preview = format_bytes(rx[:16])
        if ok:
            print(
                f"probe {idx + 1:02d}: {status} rx0=0x{resp_magic:02x} len={length} preview={preview}"
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
    rx_first = spi.xfer2(tx)
    magic_first, length_first, body_first = parse_frame(rx_first)

    responses: list[tuple[int, list[int], int, bytes]] = []
    poll_count = 1 if magic != REQ_COMMAND_MAGIC else 50  # More polls to collect all bytes
    
    # Collect response bytes (1 byte per transaction due to 1-byte FIFO)
    response_bytes = bytearray()
    
    for idx in range(poll_count):
        if magic == REQ_COMMAND_MAGIC:
            time.sleep(0.01)  # Reduced sleep for faster collection
        rx = spi.xfer2(build_frame(b"", REQ_MAGIC))
        resp_magic, resp_len, resp_body = parse_frame(rx)
        responses.append((resp_magic, rx, resp_len, resp_body))
        
        # Collect the byte from this transaction
        if resp_body and len(resp_body) > 0:
            response_bytes.extend(resp_body)
        
        # Stop if we have collected enough bytes (assuming response < 50 bytes)
        if len(response_bytes) > 1 and response_bytes[0] in (RESP_MAGIC, RESP_COMMAND_MAGIC):
            # First byte is magic, second is length
            response_length = response_bytes[1] if len(response_bytes) > 1 else 0
            # Collect magic + length + response data
            if len(response_bytes) >= response_length + 2:
                break

    print(f"frame tx: {format_bytes(orig[: min(24, len(orig))])} ...")
    print(f"frame rx1: {format_bytes(rx_first[: min(24, len(rx_first))])} ...")
    for idx, (resp_magic, rx, resp_len, _) in enumerate(responses, start=2):
        print(f"frame rx{idx}: {format_bytes(rx[: min(24, len(rx))])} ...")
    
    if magic == REQ_COMMAND_MAGIC:
        print(
            f"first transfer response: magic=0x{magic_first:02x} len={length_first}"
            if magic_first
            else "first transfer response: invalid"
        )
    
    # Parse collected response bytes
    valid = False
    response_magic = 0
    response_length = 0
    final_body = b""
    
    if len(response_bytes) >= 2:
        response_magic = response_bytes[0]
        response_length = response_bytes[1]
        if len(response_bytes) > 2:
            final_body = bytes(response_bytes[2:2 + response_length])
        valid = response_magic in (RESP_MAGIC, RESP_COMMAND_MAGIC)
    
    print(f"valid response: {'yes' if valid else 'no'}")
    print(f"response magic: 0x{response_magic:02x}")
    print(f"declared length: {response_length}")
    if final_body:
        try:
            decoded = final_body.decode('utf-8', errors='replace')
            print(f"payload: {decoded!r}")
        except Exception:
            print(f"payload: {final_body.hex()}")
    else:
        print("payload: b''")
    return 0 if valid else 1


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
