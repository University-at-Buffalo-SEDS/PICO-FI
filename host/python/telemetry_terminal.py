#!/usr/bin/env python3
"""Interactive telemetry terminal for Pico-Fi UART and SPI links."""

from __future__ import annotations

import argparse
import atexit
import queue
import select
import shutil
import socket
import sys
import termios
import threading
import time
import tty
from dataclasses import dataclass, field

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


@dataclass
class PromptState:
    prompt: str = "> "
    buffer: str = ""
    lock: threading.Lock = field(default_factory=threading.Lock)

    def _rows_for(self, text: str) -> int:
        cols = max(shutil.get_terminal_size(fallback=(80, 24)).columns, 1)
        width = max(len(text), 1)
        return (width - 1) // cols + 1

    def _clear_prompt(self) -> None:
        rows = self._rows_for(self.prompt + self.buffer)
        for idx in range(rows):
            if idx:
                sys.stdout.write("\x1b[1A")
            sys.stdout.write("\r\033[2K")

    def redraw(self) -> None:
        self._clear_prompt()
        sys.stdout.write(self.prompt + self.buffer)
        sys.stdout.flush()

    def print_line(self, line: str) -> None:
        with self.lock:
            self._clear_prompt()
            sys.stdout.write(line + "\n")
            self.redraw()

    def handle_key(self, ch: str) -> str | None:
        with self.lock:
            if ch in ("\r", "\n"):
                line = self.buffer
                self.buffer = ""
                self._clear_prompt()
                sys.stdout.flush()
                return line
            if ch in ("\x7f", "\b"):
                self.buffer = self.buffer[:-1]
            elif ch.isprintable():
                self.buffer += ch
            self.redraw()
            return None


class TerminalModeGuard:
    def __init__(self) -> None:
        self._fd: int | None = None
        self._old: list | None = None
        self._active = False

    def enable(self) -> None:
        if not sys.stdin.isatty():
            return
        fd = sys.stdin.fileno()
        old = termios.tcgetattr(fd)
        tty.setcbreak(fd)
        self._fd = fd
        self._old = old
        self._active = True

    def restore(self) -> None:
        if not self._active or self._fd is None or self._old is None:
            return
        try:
            termios.tcsetattr(self._fd, termios.TCSADRAIN, self._old)
        finally:
            self._active = False
            self._fd = None
            self._old = None


def input_loop(outbound: "queue.Queue[str]", prompt: PromptState) -> None:
    fd = sys.stdin.fileno()
    try:
        prompt.redraw()
        while True:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if not ready:
                continue
            ch = sys.stdin.read(1)
            if ch == "\x1b":
                ready, _, _ = select.select([fd], [], [], 0.01)
                if ready:
                    sys.stdin.read(1)
                    ready, _, _ = select.select([fd], [], [], 0.01)
                    if ready:
                        sys.stdin.read(1)
                continue
            line = prompt.handle_key(ch)
            if line is not None:
                outbound.put(line)
    except (EOFError, OSError):
        return


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


def print_help(prompt: PromptState) -> None:
    prompt.print_line("telemetry mode:")
    prompt.print_line("  plain text  send one telemetry payload")
    prompt.print_line("  //help      show this help")
    prompt.print_line("  //quit      exit")


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
    parser.add_argument("--poll-ms", type=int, default=50)
    backends = parser.add_subparsers(dest="backend", required=True)

    uart = backends.add_parser("uart")
    uart.add_argument("--port", required=True)
    uart.add_argument("--speed", type=int, default=115200)

    spi = backends.add_parser("spi")
    spi.add_argument("--bus", type=int, default=0)
    spi.add_argument("--device", type=int, default=0)
    spi.add_argument("--speed", type=int, default=100_000)

    args = parser.parse_args()

    sedsprintf = load_sedsprintf()
    adapter = build_adapter(args)
    prompt = PromptState()
    term_guard = TerminalModeGuard()
    atexit.register(term_guard.restore)
    term_guard.enable()

    prompt.print_line(f"telemetry terminal on {args.backend}; sender={args.sender}")
    print_help(prompt)

    outbound: queue.Queue[str] = queue.Queue()
    reader = threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True)
    reader.start()

    try:
        while True:
            try:
                line = outbound.get(timeout=max(args.poll_ms / 1000.0, 0.05))
            except queue.Empty:
                line = None

            if line is not None:
                if line == "//quit":
                    return 0
                if line == "//help":
                    print_help(prompt)
                elif line:
                    payload = build_packet(args, line)
                    if len(payload) > adapter.payload_limit:
                        prompt.print_line(
                            f"[error] telemetry packet too large: {len(payload)} > {adapter.payload_limit}"
                        )
                    else:
                        adapter.send_payload(payload)
                        prompt.print_line(f"[tx] {args.sender}: {line}")

            incoming = adapter.recv_payload(max(args.poll_ms / 1000.0, 0.05))
            if not incoming:
                continue
            packet = decode_armored_packet(sedsprintf, incoming)
            if packet is None:
                prompt.print_line(f"[skip] non-sedsprintf payload: {render_payload(incoming)!r}")
                continue
            payload = bytes(packet.payload)
            text = payload.decode("utf-8", errors="replace")
            prompt.print_line(f"[rx] {packet.sender}: {text}")
    except KeyboardInterrupt:
        return 0
    finally:
        term_guard.restore()
        adapter.close()


if __name__ == "__main__":
    raise SystemExit(main())
