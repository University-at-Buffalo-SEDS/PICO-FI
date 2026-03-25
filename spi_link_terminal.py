#!/usr/bin/env python3

from __future__ import annotations

import argparse
import queue
import sys
import threading
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
RESP_MAGIC = 0x5A


def build_frame(payload: bytes) -> list[int]:
    payload = payload[:PAYLOAD_MAX]
    frame = [0] * FRAME_SIZE
    frame[0] = REQ_MAGIC
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return frame


def parse_frame(frame: list[int]) -> bytes:
    if len(frame) != FRAME_SIZE or frame[0] != RESP_MAGIC:
        return b""
    length = min(frame[1], PAYLOAD_MAX)
    return bytes(frame[2 : 2 + length])


def print_help() -> None:
    print("chat mode:")
    print("  plain text   send to the remote peer")
    print("  /help        ask the local Pico for command help")
    print("  /show        show the local Pico config")
    print("  /ping        ping the local Pico")
    print("  /link        show the local Pico link state")
    print("  //help       show this app help")
    print("  //quit       exit the app")


def stdin_thread(outbound: "queue.Queue[bytes]") -> None:
    while True:
        line = sys.stdin.readline()
        if line == "":
            outbound.put(b"")
            return
        stripped = line.strip()
        if stripped == "//help":
            outbound.put(b"//__local_help__\n")
            continue
        if stripped == "//quit":
            outbound.put(b"")
            return
        outbound.put(line.encode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description="Interactive SPI master terminal for the Pico SPI-slave bridge.")
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=50_000)
    parser.add_argument("--mode", type=int, default=0)
    parser.add_argument("--poll-ms", type=int, default=50)
    args = parser.parse_args()

    spi = spidev.SpiDev()
    spi.open(args.bus, args.device)
    spi.max_speed_hz = args.speed
    spi.mode = args.mode
    spi.bits_per_word = 8

    outbound: "queue.Queue[bytes]" = queue.Queue()
    threading.Thread(target=stdin_thread, args=(outbound,), daemon=True).start()

    print(
        f"connected to /dev/spidev{args.bus}.{args.device} @ {args.speed}Hz mode{args.mode}. "
        "plain text chats with the remote peer. / commands talk to the local Pico. "
        "//help for app help."
    )

    try:
        pending = b""
        while True:
            try:
                while True:
                    chunk = outbound.get_nowait()
                    if chunk == b"":
                        return 0
                    if chunk == b"//__local_help__\n":
                        print_help()
                        continue
                    pending += chunk
            except queue.Empty:
                pass

            tx = build_frame(pending)
            pending = pending[PAYLOAD_MAX:]
            rx = spi.xfer2(tx)
            payload = parse_frame(rx)
            if payload:
                sys.stdout.write(payload.decode("utf-8", errors="replace"))
                sys.stdout.flush()
            time.sleep(args.poll_ms / 1000.0)
    except KeyboardInterrupt:
        return 0
    finally:
        spi.close()


if __name__ == "__main__":
    raise SystemExit(main())
