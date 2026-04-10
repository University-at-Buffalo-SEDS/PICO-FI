#!/usr/bin/env python3
"""Shared helpers for sedsprintf-backed backend routers."""

from __future__ import annotations

import argparse
import base64
import os
import socket
import sys
import time
from pathlib import Path
from typing import Protocol

ARMOR_PREFIX = b"SP6:"


class RouterAdapter(Protocol):
    payload_limit: int

    def send_payload(self, payload: bytes) -> None: ...

    def recv_payload(self, timeout_s: float) -> bytes | None: ...

    def close(self) -> None: ...


def load_sedsprintf():
    try:
        import sedsprintf_rs_2026 as sedsprintf  # type: ignore

        return sedsprintf
    except ImportError:
        search_roots: list[Path] = []
        env_root = os.environ.get("SEDSPRINTF_PYTHON_ROOT")
        if env_root:
            search_roots.append(Path(env_root))
        here = Path(__file__).resolve()
        search_roots.extend(parent / "sedsprintf_2026" / "python-files" for parent in here.parents)
        for root in search_roots:
            if not root.exists():
                continue
            sys.path.insert(0, str(root))
            import sedsprintf_rs_2026 as sedsprintf  # type: ignore

            return sedsprintf
        raise


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
        return "local-router"


def parse_int(value: str) -> int:
    return int(value, 0)


def add_router_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--listen-host", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, default=9000)
    parser.add_argument("--forward-host", default="127.0.0.1")
    parser.add_argument("--forward-port", type=int, default=9001)
    parser.add_argument("--poll-ms", type=int, default=50)
    parser.add_argument("--sender", default=default_sender_label())
    parser.add_argument("--type", dest="packet_type", default="MESSAGE_DATA")
    parser.add_argument(
        "--endpoint",
        action="append",
        default=["GROUND_STATION"],
        help="Packet endpoint name or integer value; may be repeated",
    )
    parser.add_argument("--debug", action="store_true")


def resolve_packet_type(sedsprintf, value: str) -> int:
    packet_type = getattr(sedsprintf.DataType, value, None)
    if packet_type is None:
        packet_type = parse_int(value)
    return int(packet_type)


def resolve_endpoints(sedsprintf, values: list[str]) -> list[int]:
    endpoints: list[int] = []
    for endpoint in values:
        endpoint_value = getattr(sedsprintf.DataEndpoint, endpoint, None)
        if endpoint_value is None:
            endpoint_value = parse_int(endpoint)
        endpoints.append(int(endpoint_value))
    return endpoints


def armor_packet(packet) -> bytes:
    encoded = packet.serialize()
    return ARMOR_PREFIX + base64.urlsafe_b64encode(encoded)


def decode_armored_packet(sedsprintf, payload: bytes):
    if not payload.startswith(ARMOR_PREFIX):
        return None
    encoded = payload[len(ARMOR_PREFIX) :]
    try:
        raw = base64.urlsafe_b64decode(encoded)
        return sedsprintf.deserialize_packet_py(raw)
    except Exception:
        return None


def render_payload(payload: bytes) -> str:
    try:
        return payload.decode("utf-8")
    except UnicodeDecodeError:
        return payload.hex(" ")


def run_udp_router(adapter: RouterAdapter, args: argparse.Namespace, backend_name: str) -> int:
    sedsprintf = load_sedsprintf()
    packet_type = resolve_packet_type(sedsprintf, args.packet_type)
    endpoints = resolve_endpoints(sedsprintf, args.endpoint)

    listen = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listen.bind((args.listen_host, args.listen_port))
    listen.setblocking(False)
    forward = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    print(
        f"{backend_name} router listening on udp://{args.listen_host}:{args.listen_port} "
        f"and forwarding received payloads to udp://{args.forward_host}:{args.forward_port}"
    )
    print(f"sender={args.sender} packet_type={packet_type} endpoints={endpoints}")

    try:
        while True:
            try:
                data, source = listen.recvfrom(65535)
            except BlockingIOError:
                data = b""
                source = None

            if source is not None:
                packet = sedsprintf.make_packet(
                    packet_type,
                    args.sender,
                    endpoints,
                    int(time.time() * 1000),
                    data,
                )
                armored = armor_packet(packet)
                if len(armored) > adapter.payload_limit:
                    print(
                        f"[drop] local udp payload too large for {backend_name}: "
                        f"{len(armored)} > {adapter.payload_limit}"
                    )
                else:
                    adapter.send_payload(armored)
                    if args.debug:
                        print(
                            f"[tx] {source[0]}:{source[1]} -> {backend_name} "
                            f"{len(data)}B {render_payload(data)!r}"
                        )

            incoming = adapter.recv_payload(args.poll_ms / 1000.0)
            if incoming:
                packet = decode_armored_packet(sedsprintf, incoming)
                if packet is None:
                    if args.debug:
                        print(f"[skip] non-sedsprintf payload: {render_payload(incoming)!r}")
                else:
                    payload = bytes(packet.payload)
                    forward.sendto(payload, (args.forward_host, args.forward_port))
                    print(
                        f"[rx] {packet.sender} -> udp://{args.forward_host}:{args.forward_port} "
                        f"{len(payload)}B {render_payload(payload)!r}"
                    )

            if source is None and incoming is None:
                time.sleep(max(args.poll_ms / 1000.0, 0.01))
    except KeyboardInterrupt:
        return 0
    finally:
        adapter.close()
        listen.close()
        forward.close()
