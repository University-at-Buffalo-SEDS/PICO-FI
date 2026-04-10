#!/usr/bin/env python3
"""Interactive telemetry terminal for Pico-Fi UART and SPI links."""

from __future__ import annotations

import argparse
import socket
import sys
import threading
import time

try:
    from .telemetry_cli import build_adapter
    from .sedsprintf_router_common import (
        armor_packet,
        decode_armored_packet,
        load_sedsprintf,
        render_payload,
        resolve_endpoints,
        resolve_packet_type,
    )
except ImportError:
    import os

    sys.path.insert(0, os.path.dirname(__file__))
    from telemetry_cli import build_adapter
    from sedsprintf_router_common import (
        armor_packet,
        decode_armored_packet,
        load_sedsprintf,
        render_payload,
        resolve_endpoints,
        resolve_packet_type,
    )


def default_sender_label() -> str:
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            sock.connect(("192.0.2.1", 1))
            return sock.getsockname()[0]
        finally:
            sock.close()
    except OSError:
        pass
    try:
        return socket.gethostbyname(socket.gethostname())
    except OSError:
        return "telemetry-terminal"


class Printer:
    def __init__(self) -> None:
        self.lock = threading.Lock()

    def line(self, text: str) -> None:
        with self.lock:
            print(text, flush=True)

    def prompt(self) -> None:
        with self.lock:
            sys.stdout.write("> ")
            sys.stdout.flush()


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


def print_help(printer: Printer) -> None:
    printer.line("telemetry mode:")
    printer.line("  plain text  send one telemetry payload")
    printer.line("  //help      show this help")
    printer.line("  //quit      exit")


def recv_loop(
    adapter,
    adapter_lock: threading.Lock,
    printer: Printer,
    stop_event: threading.Event,
    poll_s: float,
) -> None:
    sedsprintf = load_sedsprintf()
    while not stop_event.is_set():
        try:
            with adapter_lock:
                incoming = adapter.recv_payload(poll_s)
        except Exception as exc:
            printer.line(f"[error] receive failed: {exc}")
            stop_event.set()
            return
        if not incoming:
            continue
        packet = decode_armored_packet(sedsprintf, incoming)
        if packet is None:
            printer.line(f"[skip] non-sedsprintf payload: {render_payload(incoming)!r}")
            continue
        payload = bytes(packet.payload).decode("utf-8", errors="replace")
        printer.line(f"[rx] {packet.sender}: {payload}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sender", default=default_sender_label())
    parser.add_argument("--type", dest="packet_type", default="MESSAGE_DATA")
    parser.add_argument(
        "--endpoint",
        action="append",
        default=["GROUND_STATION"],
        help="Packet endpoint name or integer value; may be repeated",
    )
    parser.add_argument("--poll-ms", type=int, default=5)
    backends = parser.add_subparsers(dest="backend", required=True)

    uart = backends.add_parser("uart")
    uart.add_argument("--port", required=True)
    uart.add_argument("--speed", type=int, default=115200)

    spi = backends.add_parser("spi")
    spi.add_argument("--bus", type=int, default=0)
    spi.add_argument("--device", type=int, default=0)
    spi.add_argument("--speed", type=int, default=100_000)

    args = parser.parse_args()

    adapter = build_adapter(args)
    adapter_lock = threading.Lock()
    printer = Printer()
    stop_event = threading.Event()
    poll_s = max(args.poll_ms / 1000.0, 0.05)

    printer.line(f"telemetry terminal on {args.backend}; sender={args.sender}")
    print_help(printer)

    receiver = threading.Thread(
        target=recv_loop,
        args=(adapter, adapter_lock, printer, stop_event, poll_s),
        daemon=True,
    )
    receiver.start()

    try:
        while not stop_event.is_set():
            printer.prompt()
            line = sys.stdin.readline()
            if not line:
                break
            line = line.rstrip("\r\n")
            if not line:
                continue
            if line == "//quit":
                break
            if line == "//help":
                print_help(printer)
                continue

            payload = build_packet(args, line)
            if len(payload) > adapter.payload_limit:
                printer.line(
                    f"[error] telemetry packet too large: {len(payload)} > {adapter.payload_limit}"
                )
                continue
            try:
                with adapter_lock:
                    adapter.send_payload(payload)
            except Exception as exc:
                printer.line(f"[error] send failed: {exc}")
                return 1
            printer.line(f"[tx] {args.sender}: {line}")
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        receiver.join(timeout=1.0)
        adapter.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
