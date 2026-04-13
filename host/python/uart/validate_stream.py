#!/usr/bin/env python3
"""Validate UART framing using the same resync rules as the Pico firmware."""

from __future__ import annotations

import argparse

import serial
import time

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2

REQ_DATA_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def open_serial(port: str, speed: int) -> serial.Serial:
    return serial.Serial(
        port=port,
        baudrate=speed,
        timeout=0.05,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_ONE,
        xonxoff=False,
        rtscts=False,
        dsrdtr=False,
    )


def format_bytes(data: bytes, limit: int = 32) -> str:
    preview = " ".join(f"{b:02x}" for b in data[:limit])
    if len(data) > limit:
        return preview + " ..."
    return preview


def is_valid_magic(byte: int, mode: str) -> bool:
    if mode == "request":
        return byte in (REQ_DATA_MAGIC, REQ_COMMAND_MAGIC)
    return byte in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC)


class FirmwareStyleValidator:
    def __init__(self, mode: str) -> None:
        self.mode = mode
        self.buf = bytearray()

    def push_byte(self, byte: int) -> tuple[str, str] | None:
        if not self.buf:
            if not is_valid_magic(byte, self.mode):
                return ("skip", f"discard stray byte 0x{byte:02x}")
            self.buf.append(byte)
            return ("sync", f"start magic 0x{byte:02x}")

        if len(self.buf) == 1:
            self.buf.append(byte)
            if byte > PAYLOAD_MAX:
                msg = f"invalid length {byte}, resync"
                if is_valid_magic(byte, self.mode):
                    self.buf[:] = bytes([byte])
                    return ("resync", msg + f" on magic 0x{byte:02x}")
                self.buf.clear()
                return ("reject", msg)
            return ("len", f"length={byte}")

        self.buf.append(byte)
        if len(self.buf) < FRAME_SIZE:
            return None

        frame = bytes(self.buf)
        self.buf.clear()

        payload_len = frame[1]
        if payload_len > PAYLOAD_MAX:
            return ("reject", f"frame length out of range: {payload_len}")

        pad = frame[2 + payload_len:]
        if any(byte != 0 for byte in pad):
            first_bad = next(idx for idx, value in enumerate(pad, start=2 + payload_len) if value != 0)
            return (
                "reject",
                f"non-zero padding at offset {first_bad}: {frame[first_bad]:02x}; "
                f"frame={format_bytes(frame, 48)}",
            )

        magic = frame[0]
        payload = frame[2: 2 + payload_len]
        try:
            rendered = payload.decode("utf-8")
            payload_text = f" text={rendered!r}"
        except UnicodeDecodeError:
            payload_text = ""
        return (
            "frame",
            f"magic=0x{magic:02x} len={payload_len} payload={format_bytes(payload)}{payload_text}",
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", default="/dev/cu.usbmodem11102")
    parser.add_argument("--speed", type=int, default=115200)
    parser.add_argument(
        "--mode",
        choices=("request", "response"),
        default="request",
        help="Validate as Pico-input request frames or Pico-output response frames.",
    )
    parser.add_argument(
        "--show-skips",
        action="store_true",
        help="Print stray bytes that are ignored while waiting for frame sync.",
    )
    args = parser.parse_args()

    validator = FirmwareStyleValidator(args.mode)
    print(f"validating {args.mode} frames on {args.port} @ {args.speed}")

    try:
        with open_serial(args.port, args.speed) as ser:
            while True:
                chunk = ser.read(256)
                if not chunk:
                    continue
                for byte in chunk:
                    result = validator.push_byte(byte)
                    if result is None:
                        continue
                    kind, message = result
                    if kind == "skip" and not args.show_skips:
                        continue
                    timestamp = time.strftime("%H:%M:%S")
                    print(f"[{timestamp}] {kind}: {message}")
    except KeyboardInterrupt:
        return 0
    except serial.SerialException as exc:
        print(f"error: serial failure on {args.port}: {exc}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
