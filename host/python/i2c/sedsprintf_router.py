#!/usr/bin/env python3
"""Route local UDP datagrams over Pico-Fi I2C using sedsprintf packets."""

from __future__ import annotations

import argparse
from collections import deque

try:
    from ..sedsprintf_router_common import add_router_args, run_udp_router
    from .protocol import KIND_DATA, read_packet, write_packet
    from .raw import open_bus
except ImportError:
    import os
    import sys

    sys.path.append(os.path.dirname(os.path.dirname(__file__)))
    sys.path.append(os.path.dirname(__file__))
    from sedsprintf_router_common import add_router_args, run_udp_router
    from protocol import KIND_DATA, read_packet, write_packet
    from raw import open_bus

CHUNK_DELAY_S = 0.001


class I2cRouterAdapter:
    payload_limit = 240

    def __init__(self, bus_num: int, addr: int) -> None:
        self.bus = open_bus(bus_num)
        self.addr = addr
        self.transfer_id = 1
        self.pending: deque[bytes] = deque()

    def _next_transfer_id(self) -> int:
        value = self.transfer_id
        self.transfer_id = (self.transfer_id + 1) & 0xFFFF or 1
        return value

    def send_payload(self, payload: bytes) -> None:
        write_packet(
            self.bus,
            self.addr,
            payload,
            kind=KIND_DATA,
            transfer_id=self._next_transfer_id(),
            chunk_delay_s=CHUNK_DELAY_S,
        )
        response = read_packet(self.bus, self.addr, chunk_delay_s=CHUNK_DELAY_S, timeout_s=0.05)
        if response is not None:
            rx_kind, rx_payload = response
            if rx_kind == KIND_DATA and rx_payload:
                self.pending.append(rx_payload)

    def recv_payload(self, timeout_s: float) -> bytes | None:
        if self.pending:
            return self.pending.popleft()
        response = read_packet(self.bus, self.addr, chunk_delay_s=CHUNK_DELAY_S, timeout_s=timeout_s)
        if response is None:
            return None
        rx_kind, rx_payload = response
        if rx_kind == KIND_DATA and rx_payload:
            return rx_payload
        return None

    def close(self) -> None:
        self.bus.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bus", type=int, default=1)
    parser.add_argument("--addr", type=lambda value: int(value, 0), default=0x55)
    add_router_args(parser)
    args = parser.parse_args()
    return run_udp_router(I2cRouterAdapter(args.bus, args.addr), args, "i2c")


if __name__ == "__main__":
    raise SystemExit(main())
