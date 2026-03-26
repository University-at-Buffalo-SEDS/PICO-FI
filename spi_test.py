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
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    magic = frame[0]
    if magic not in (RESP_MAGIC, RESP_COMMAND_MAGIC):
        return 0, frame[1] if len(frame) > 1 else 0, b""
    
    # Parse length byte - if it's 0xFF or clearly invalid, it might be uninitialized
    length_byte = frame[1]
    length = min(length_byte, PAYLOAD_MAX)
    
    # Extract payload and filter out invalid UTF-8
    raw_payload = bytes(frame[2 : 2 + length])
    
    # If length is suspiciously large (0xFF) or all bytes are the same (0xFF pattern),
    # try to detect actual end of valid data
    if length_byte == 0xFF and raw_payload:
        # Find the first null byte or pattern change
        for i, byte_val in enumerate(raw_payload):
            if byte_val == 0x00 or byte_val < 0x20 and byte_val != 0x0A and byte_val != 0x0D:
                length = i
                raw_payload = raw_payload[:i]
                break
    
    return magic, length, raw_payload


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
    poll_count = 1 if magic != REQ_COMMAND_MAGIC else 20  # More polls for chunked responses
    
    # Collect response chunks across multiple transactions
    response_magic = 0
    response_length = 0
    response_data = bytearray()
    got_header = False
    
    for _ in range(poll_count):
        if magic == REQ_COMMAND_MAGIC:
            time.sleep(0.02)
        rx = spi.xfer2(build_frame(b"", REQ_MAGIC))
        resp_magic, resp_len, resp_body = parse_frame(rx)
        responses.append((resp_magic, rx, resp_len, resp_body))
        
        # Handle chunked response format: first frame has [magic, length], rest has data
        if resp_magic == RESP_COMMAND_MAGIC or resp_magic == RESP_DATA_MAGIC:
            if not got_header and resp_len >= 1 and resp_body:
                # First frame: extract magic byte and length byte
                response_magic = resp_magic
                response_length = resp_body[0]
                # Add any remaining bytes from first frame
                if len(resp_body) > 1:
                    response_data.extend(resp_body[1:])
                got_header = True
            elif got_header and resp_body:
                # Subsequent frames: add all bytes as data
                response_data.extend(resp_body)
        
        # Stop if we have collected all response data
        if got_header and len(response_data) >= response_length:
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
    
    # Validate response
    valid = got_header and response_magic in (RESP_COMMAND_MAGIC, RESP_DATA_MAGIC)
    final_body = bytes(response_data[:response_length])
    
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
