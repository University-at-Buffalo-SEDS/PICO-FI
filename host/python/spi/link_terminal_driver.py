#!/usr/bin/env python3
"""Non-interactive driver for link_terminal SPI transaction logic."""

from __future__ import annotations

import argparse
import sys
import time

try:
    from .link_terminal import (
        REQ_COMMAND_MAGIC,
        REQ_MAGIC,
        RESP_DATA_MAGIC,
        StreamPrinter,
        TransactionThrottle,
        default_sender_label,
        exchange_frame,
        poll_inbound_data,
        format_outbound_chat,
    )
    from .raw import open_bus
except ImportError:
    import os

    sys.path.append(os.path.dirname(__file__))
    from link_terminal import (
        REQ_COMMAND_MAGIC,
        REQ_MAGIC,
        RESP_DATA_MAGIC,
        StreamPrinter,
        TransactionThrottle,
        default_sender_label,
        exchange_frame,
        poll_inbound_data,
        format_outbound_chat,
    )
    from raw import open_bus


class CapturePrompt:
    def __init__(self) -> None:
        self.lines: list[str] = []

    def print_line(self, line: str) -> None:
        self.lines.append(line)
        print(line, flush=True)


def has_failure(lines: list[str]) -> bool:
    return any(
        line.startswith("[spi dbg]")
        or line.startswith("[error]")
        or "timed out waiting for SPI reply" in line
        for line in lines
    )


def poll_once(
    bus_num: int,
    device: int,
    speed: int,
    throttle: TransactionThrottle,
    stream_printer: StreamPrinter,
    poll_delay_s: float,
) -> None:
    payload = poll_inbound_data(bus_num, device, speed, poll_delay_s, throttle)
    if payload:
        stream_printer.feed(payload)
        stream_printer.flush_partial()


def main() -> int:
    parser = argparse.ArgumentParser(description="Drive link_terminal SPI transactions without a TTY.")
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=100_000)
    parser.add_argument("--poll-ms", type=int, default=50)
    parser.add_argument("--recv-timeout-s", type=float, default=12.0)
    parser.add_argument("--sender", default="")
    args = parser.parse_args()

    sender = args.sender or default_sender_label()
    prompt = CapturePrompt()
    stream_printer = StreamPrinter(prompt=prompt)
    throttle = TransactionThrottle(min_gap_s=max(args.poll_ms / 1000.0, 0.05))
    poll_delay_s = args.poll_ms / 1000.0

    print("READY", flush=True)
    for raw_line in sys.stdin:
        line = raw_line.rstrip("\n")
        if line == "quit":
            print("BYE", flush=True)
            return 0
        if line.startswith("send "):
            start = len(prompt.lines)
            rendered = format_outbound_chat(sender, line[5:])
            exchange_frame(
                args.bus,
                args.device,
                args.speed,
                prompt,
                stream_printer,
                REQ_MAGIC,
                rendered.encode("utf-8"),
                poll_delay_s,
                False,
                throttle,
            )
            emitted = prompt.lines[start:]
            if has_failure(emitted):
                print(f"FAIL send {rendered}", flush=True)
                return 1
            print(f"OK send {rendered}", flush=True)
            continue
        if line.startswith("command "):
            start = len(prompt.lines)
            command = line[8:]
            exchange_frame(
                args.bus,
                args.device,
                args.speed,
                prompt,
                stream_printer,
                REQ_COMMAND_MAGIC,
                (command + "\n").encode("utf-8"),
                poll_delay_s,
                True,
                throttle,
            )
            emitted = prompt.lines[start:]
            if has_failure(emitted):
                print(f"FAIL command {command}", flush=True)
                return 1
            print(f"OK command {command}", flush=True)
            continue
        if line.startswith("recv "):
            start = len(prompt.lines)
            expect = line[5:]
            deadline = time.monotonic() + args.recv_timeout_s
            while time.monotonic() < deadline:
                poll_once(
                    args.bus,
                    args.device,
                    args.speed,
                    throttle,
                    stream_printer,
                    poll_delay_s,
                )
                emitted = prompt.lines[start:]
                if has_failure(emitted):
                    print(f"FAIL recv {expect}", flush=True)
                    return 1
                for item in emitted:
                    if expect in item:
                        print(f"MATCH {item}", flush=True)
                        break
                else:
                    time.sleep(max(poll_delay_s, 0.05))
                    continue
                break
            else:
                print(f"FAIL recv {expect}", flush=True)
                return 1
            continue

        print(f"FAIL unknown {line}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
