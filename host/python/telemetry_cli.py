#!/usr/bin/env python3
"""One-shot telemetry send/receive CLI for Pico-Fi UART and SPI links."""

from __future__ import annotations

import argparse

import sys
import time

try:
    from .sedsprintf_router_common import (
        armor_packet,
        decode_packet,
        load_sedsprintf,
        render_payload,
        resolve_endpoints,
        resolve_packet_type,
    )
except ImportError:
    import os

    sys.path.append(os.path.dirname(__file__))
    from sedsprintf_router_common import (
        armor_packet,
        decode_packet,
        load_sedsprintf,
        render_payload,
        resolve_endpoints,
        resolve_packet_type,
    )


def add_packet_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--sender", default="telemetry-cli")
    parser.add_argument("--type", dest="packet_type", default="MESSAGE_DATA")
    parser.add_argument(
        "--endpoint",
        action="append",
        default=["GROUND_STATION"],
        help="Packet endpoint name or integer value; may be repeated",
    )


def add_backend_args(parser: argparse.ArgumentParser) -> None:
    backend = parser.add_subparsers(dest="backend", required=True)

    uart = backend.add_parser("uart", help="Use the local UART transport")
    uart.add_argument("--port", required=True)
    uart.add_argument("--speed", type=int, default=115200)

    spi = backend.add_parser("spi", help="Use the local SPI transport")
    spi.add_argument("--bus", type=int, default=0)
    spi.add_argument("--device", type=int, default=0)
    spi.add_argument("--speed", type=int, default=100_000)


def build_adapter(args: argparse.Namespace):
    if args.backend == "uart":
        try:
            from .uart.sedsprintf_router import UartRouterAdapter
        except ImportError:
            from uart.sedsprintf_router import UartRouterAdapter
        return UartRouterAdapter(args.port, args.speed)
    if args.backend == "spi":
        try:
            from .spi.sedsprintf_router import SpiRouterAdapter
        except ImportError:
            from spi.sedsprintf_router import SpiRouterAdapter
        return SpiRouterAdapter(args.bus, args.device, args.speed)
    raise ValueError(f"unsupported backend {args.backend!r}")


def build_packet(args: argparse.Namespace, payload_text: str) -> bytes:
    sedsprintf = load_sedsprintf()
    packet_type = resolve_packet_type(sedsprintf, args.packet_type)
    endpoints = resolve_endpoints(sedsprintf, args.endpoint)
    packet = sedsprintf.make_packet(
        packet_type,
        args.sender,
        endpoints,
        int(time.time() * 1000),
        payload_text.encode("utf-8"),
    )
    return armor_packet(packet)


def run_send(args: argparse.Namespace) -> int:
    payload = build_packet(args, args.text)
    adapter = build_adapter(args)
    try:
        if len(payload) > adapter.payload_limit:
            print(
                f"ERROR: telemetry packet too large for {args.backend}: "
                f"{len(payload)} > {adapter.payload_limit}",
                file=sys.stderr,
            )
            return 1
        adapter.send_payload(payload)
    finally:
        adapter.close()

    print(f"sent via {args.backend}: {args.text}")
    return 0


def run_recv(args: argparse.Namespace) -> int:
    sedsprintf = load_sedsprintf()
    adapter = build_adapter(args)
    deadline = time.monotonic() + args.timeout
    try:
        while time.monotonic() < deadline:
            incoming = adapter.recv_payload(min(args.poll_interval, max(deadline - time.monotonic(), 0.01)))
            if not incoming:
                continue
            packet = decode_packet(sedsprintf, incoming)
            if packet is None:
                print(f"skip non-sedsprintf payload: {render_payload(incoming)!r}", file=sys.stderr)
                continue
            payload = bytes(packet.payload)
            text = payload.decode("utf-8", errors="replace")
            print(f"received via {args.backend}: sender={packet.sender} payload={text}")
            if args.expect and args.expect not in text:
                print(
                    f"ERROR: expected payload containing {args.expect!r}, got {text!r}",
                    file=sys.stderr,
                )
                return 1
            return 0
    finally:
        adapter.close()

    print(f"ERROR: timed out waiting for telemetry on {args.backend}", file=sys.stderr)
    return 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    send = subparsers.add_parser("send", help="Send one telemetry payload")
    add_packet_args(send)
    send.add_argument("text")
    add_backend_args(send)

    recv = subparsers.add_parser("recv", help="Wait for and print one telemetry payload")
    recv.add_argument("--timeout", type=float, default=10.0)
    recv.add_argument("--poll-interval", type=float, default=0.25)
    recv.add_argument("--expect", default="")
    add_backend_args(recv)

    args = parser.parse_args()
    if args.command == "send":
        return run_send(args)
    if args.command == "recv":
        return run_recv(args)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
