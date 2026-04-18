#!/usr/bin/env python3
"""Route local UDP datagrams over Pico-Fi SPI using sedsprintf packets."""

from __future__ import annotations

import argparse
from collections import deque

import time

try:
    from ..sedsprintf_router_common import add_router_args, run_udp_router
    from .raw import FRAME_SIZE, open_bus
    from .test import (
        COMMAND_POLL_LIMIT,
        PAYLOAD_MAX,
        PULL_COMMAND,
        REQ_COMMAND_MAGIC,
        REQ_MAGIC,
        RESP_MAGIC,
        build_frame,
        parse_frame,
    )
except ImportError:
    import os
    import sys

    sys.path.insert(0, os.path.dirname(__file__))
    sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
    from sedsprintf_router_common import add_router_args, run_udp_router
    from raw import FRAME_SIZE, open_bus
    from test import (
        COMMAND_POLL_LIMIT,
        PAYLOAD_MAX,
        PULL_COMMAND,
        REQ_COMMAND_MAGIC,
        REQ_MAGIC,
        RESP_MAGIC,
        build_frame,
        parse_frame,
    )


class SpiRouterAdapter:
    payload_limit = PAYLOAD_MAX
    send_poll_attempts = 2
    send_poll_sleep_s = 0.002
    poll_sleep_s = 0.002

    def __init__(self, bus_num: int, device: int, speed: int) -> None:
        self.bus = open_bus(bus_num, device, speed)
        self.pending: deque[bytes] = deque()

    def _capture(self, frame: bytes) -> bytes | None:
        magic, _, payload = parse_frame(frame)
        if magic == RESP_MAGIC and payload:
            return payload
        return None

    def send_payload(self, payload: bytes) -> None:
        first = self.bus.write_frame(build_frame(payload, REQ_MAGIC))
        captured = self._capture(first)
        if captured:
            self.pending.append(captured)
            return
        for _ in range(self.send_poll_attempts):
            time.sleep(self.send_poll_sleep_s)
            captured = self._capture(self.bus.read_frame())
            if captured:
                self.pending.append(captured)
                break

    def recv_payload(self, timeout_s: float) -> bytes | None:
        if self.pending:
            return self.pending.popleft()
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            captured = self._capture(
                self.bus.write_frame(build_frame(PULL_COMMAND, REQ_COMMAND_MAGIC))
            )
            if captured:
                return captured
            for _ in range(COMMAND_POLL_LIMIT):
                captured = self._capture(self.bus.read_frame())
                if captured:
                    return captured
                if time.monotonic() >= deadline:
                    return None
                time.sleep(self.poll_sleep_s)
        return None

    def close(self) -> None:
        self.bus.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=100_000)
    add_router_args(parser)
    args = parser.parse_args()
    return run_udp_router(SpiRouterAdapter(args.bus, args.device, args.speed), args, "spi")


if __name__ == "__main__":
    raise SystemExit(main())
