#!/usr/bin/env python3

from __future__ import annotations

import argparse
import sys
import threading
import time

try:
    import serial
except ImportError as exc:  # pragma: no cover - runtime dependency
    raise SystemExit(
        "error: pyserial is required. Install it with `python3 -m pip install pyserial`."
    ) from exc


def read_loop(ser: serial.Serial) -> None:
    while True:
        try:
            data = ser.read(1)
        except serial.SerialException as exc:
            print(f"\n[serial read error] {exc}", file=sys.stderr)
            return
        if not data:
            continue
        sys.stdout.write(data.decode("utf-8", errors="replace"))
        sys.stdout.flush()


def write_loop(ser: serial.Serial) -> int:
    try:
        while True:
            line = sys.stdin.readline()
            if line == "":
                return 0
            ser.write(line.encode("utf-8"))
            ser.flush()
    except KeyboardInterrupt:
        return 0
    except serial.SerialException as exc:
        print(f"\n[serial write error] {exc}", file=sys.stderr)
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Interactive UART terminal for the two-Pico network bridge."
    )
    parser.add_argument("--port", required=True, help="Serial device, e.g. /dev/cu.usbmodemXXXX or /dev/serial0")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--label", default="", help="Optional label printed on startup.")
    args = parser.parse_args()

    try:
        ser = serial.Serial(args.port, args.baud, timeout=0.1)
    except serial.SerialException as exc:
        print(f"error: failed to open {args.port}: {exc}", file=sys.stderr)
        return 1

    with ser:
        if args.label:
            print(f"[{args.label}] connected to {args.port} @ {args.baud}")
        else:
            print(f"connected to {args.port} @ {args.baud}")
        print("type text and press Enter. Ctrl-C to exit.")

        reader = threading.Thread(target=read_loop, args=(ser,), daemon=True)
        reader.start()
        status = write_loop(ser)
        time.sleep(0.1)
        return status


if __name__ == "__main__":
    raise SystemExit(main())
