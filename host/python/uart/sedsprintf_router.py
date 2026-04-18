#!/usr/bin/env python3
"""Route local UDP datagrams over Pico-Fi UART using sedsprintf packets."""

from __future__ import annotations

import argparse
from collections import deque

try:
    from ..sedsprintf_router_common import add_router_args, run_udp_router
    from .test import (
        FRAME_SIZE,
        PAYLOAD_MAX,
        REQ_DATA_MAGIC,
        RESP_DATA_MAGIC,
        build_frame,
        open_serial,
        parse_frame,
        read_frame,
    )
except ImportError:
    import os
    import sys

    sys.path.insert(0, os.path.dirname(__file__))
    sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
    from sedsprintf_router_common import add_router_args, run_udp_router
    from test import (
        FRAME_SIZE,
        PAYLOAD_MAX,
        REQ_DATA_MAGIC,
        RESP_DATA_MAGIC,
        build_frame,
        open_serial,
        parse_frame,
        read_frame,
    )


class UartRouterAdapter:
    payload_limit = PAYLOAD_MAX
    minimum_poll_s = 0.002
    send_reply_wait_s = 0.005

    def __init__(self, port: str, speed: int) -> None:
        self.ser = open_serial(port, speed)
        self.pending: deque[bytes] = deque()

    def _capture(self, frame: bytes) -> bytes | None:
        if len(frame) < 4:
            return None
        magic, _, payload = parse_frame(frame)
        if magic == RESP_DATA_MAGIC and payload:
            return payload
        return None

    def send_payload(self, payload: bytes) -> None:
        self.ser.write(build_frame(payload, REQ_DATA_MAGIC))
        self.ser.flush()
        captured = self._capture(read_frame(self.ser, self.send_reply_wait_s))
        if captured:
            self.pending.append(captured)

    def recv_payload(self, timeout_s: float) -> bytes | None:
        if self.pending:
            return self.pending.popleft()
        self.ser.write(build_frame(b"", REQ_DATA_MAGIC))
        self.ser.flush()
        captured = self._capture(read_frame(self.ser, max(timeout_s, self.minimum_poll_s)))
        if captured:
            return captured
        return None

    def close(self) -> None:
        self.ser.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", required=True)
    parser.add_argument("--speed", type=int, default=115200)
    add_router_args(parser)
    args = parser.parse_args()
    return run_udp_router(UartRouterAdapter(args.port, args.speed), args, "uart")


if __name__ == "__main__":
    raise SystemExit(main())
