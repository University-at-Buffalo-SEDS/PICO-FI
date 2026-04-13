#!/usr/bin/env python3
"""Send sedsprintf_rs_2026 packets over UART."""

from __future__ import annotations

import argparse
from pathlib import Path

import serial
import sys
import time


def load_sedsprintf():
    try:
        import sedsprintf_rs_2026 as sedsprintf  # type: ignore

        return sedsprintf
    except ImportError:
        local_pkg_root = (
                Path(__file__).resolve().parents[4] / "sedsprintf_2026" / "python-files"
        )
        if local_pkg_root.exists():
            sys.path.insert(0, str(local_pkg_root))
            import sedsprintf_rs_2026 as sedsprintf  # type: ignore

            return sedsprintf
        raise


def open_serial(port: str, baud: int) -> serial.Serial:
    return serial.Serial(
        port=port,
        baudrate=baud,
        timeout=0.1,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_ONE,
        xonxoff=False,
        rtscts=False,
        dsrdtr=False,
    )


def parse_int(value: str) -> int:
    return int(value, 0)


def parse_payload(args: argparse.Namespace) -> bytes:
    if args.payload_hex:
        cleaned = "".join(args.payload_hex.split())
        return bytes.fromhex(cleaned)
    if args.payload_text is not None:
        return args.payload_text.encode("utf-8")
    return b""


def format_bytes(data: bytes, limit: int = 32) -> str:
    return " ".join(f"{byte:02x}" for byte in data[:limit])


def read_reply(ser: serial.Serial, timeout_s: float) -> bytes:
    deadline = time.monotonic() + timeout_s
    buf = bytearray()
    while time.monotonic() < deadline:
        chunk = ser.read(512)
        if chunk:
            buf.extend(chunk)
            continue
        if buf:
            break
    return bytes(buf)


def main() -> int:
    sedsprintf = load_sedsprintf()

    parser = argparse.ArgumentParser(
        description="Build a sedsprintf_rs_2026 packet and send it over UART."
    )
    parser.add_argument("--port", required=True, help="Serial device path")
    parser.add_argument("--baud", type=int, default=115200, help="UART baud rate")
    parser.add_argument(
        "--type",
        dest="packet_type",
        default="MESSAGE_DATA",
        help="Packet type name or integer value",
    )
    parser.add_argument(
        "--endpoint",
        action="append",
        default=["GROUND_STATION"],
        help="Endpoint name or integer value; may be repeated",
    )
    parser.add_argument("--sender", default="host-uart", help="Packet sender string")
    parser.add_argument(
        "--timestamp-ms",
        type=int,
        default=None,
        help="Packet timestamp in ms; defaults to current wall clock ms",
    )
    payload_group = parser.add_mutually_exclusive_group()
    payload_group.add_argument("--payload-text", help="UTF-8 payload text")
    payload_group.add_argument("--payload-hex", help="Hex payload bytes, e.g. 'de ad be ef'")
    parser.add_argument(
        "--read-reply",
        action="store_true",
        help="Read bytes back for a short period and try to decode them as a sedsprintf packet",
    )
    parser.add_argument(
        "--reply-timeout",
        type=float,
        default=0.5,
        help="Seconds to wait for reply bytes when --read-reply is used",
    )
    args = parser.parse_args()

    packet_type = getattr(sedsprintf.DataType, args.packet_type, None)
    if packet_type is None:
        packet_type = parse_int(args.packet_type)

    endpoints: list[int] = []
    for endpoint in args.endpoint:
        endpoint_value = getattr(sedsprintf.DataEndpoint, endpoint, None)
        if endpoint_value is None:
            endpoint_value = parse_int(endpoint)
        endpoints.append(int(endpoint_value))

    payload = parse_payload(args)
    timestamp_ms = (
        args.timestamp_ms if args.timestamp_ms is not None else int(time.time() * 1000)
    )

    packet = sedsprintf.make_packet(
        int(packet_type),
        args.sender,
        endpoints,
        timestamp_ms,
        payload,
    )
    encoded = packet.serialize()

    print(f"Packet type: {int(packet_type)}")
    print(f"Sender: {args.sender}")
    print(f"Endpoints: {endpoints}")
    print(f"Payload: {format_bytes(payload)}")
    print(f"Serialized ({len(encoded)} bytes): {format_bytes(encoded)}")

    with open_serial(args.port, args.baud) as ser:
        ser.reset_input_buffer()
        ser.write(encoded)
        ser.flush()
        print(f"Sent {len(encoded)} bytes on {args.port} @ {args.baud}")

        if not args.read_reply:
            return 0

        reply = read_reply(ser, args.reply_timeout)
        if not reply:
            print("Reply: <none>")
            return 0

        print(f"Reply raw ({len(reply)} bytes): {format_bytes(reply)}")
        try:
            decoded = sedsprintf.deserialize_packet_py(reply)
        except Exception as exc:
            print(f"Reply decode failed: {exc}")
            return 1

        print(f"Reply packet: {decoded}")
        print(f"Reply sender: {decoded.sender}")
        print(f"Reply payload: {decoded.payload!r}")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
