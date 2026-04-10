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


def print_help() -> list[str]:
    return [
        "telemetry mode:",
        "  plain text  send one serialized telemetry payload",
        "  //help      show this help",
        "  //quit      exit the app",
    ]


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


def render_incoming(prompt: PromptState, packet) -> None:
    payload = bytes(packet.payload).decode("utf-8", errors="replace").replace("\r", "")
    for line in payload.split("\n"):
        if line:
            prompt.print_line(f"[rx] {packet.sender}: {line}")


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

    adapter = build_adapter(args)
    prompt = PromptState()
    outbound: "queue.Queue[str]" = queue.Queue()
    terminal = TerminalModeGuard()
    terminal.enable()
    atexit.register(terminal.restore)
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()

    print(
        f"connected to {args.backend} telemetry. "
        "plain text sends one serialized telemetry packet. "
        "//help for app help."
    )

    try:
        while True:
            try:
                while True:
                    line = outbound.get_nowait()
                    stripped = line.strip()
                    if stripped == "//help":
                        for help_line in print_help():
                            prompt.print_line(help_line)
                        continue
                    if stripped == "//quit":
                        return 0
                    if not line:
                        continue
                    payload = build_packet(args, line)
                    if len(payload) > adapter.payload_limit:
                        prompt.print_line(
                            f"[error] telemetry packet too large: {len(payload)} > {adapter.payload_limit}"
                        )
                        continue
                    adapter.send_payload(payload)
                    prompt.print_line(f"[tx] {args.sender}: {line}")
            except queue.Empty:
                pass

            incoming = adapter.recv_payload(max(args.poll_ms / 1000.0, 0.01))
            if not incoming:
                continue
            packet = decode_armored_packet(load_sedsprintf(), incoming)
            if packet is None:
                prompt.print_line(f"[skip] non-sedsprintf payload: {render_payload(incoming)!r}")
                continue
            render_incoming(prompt, packet)
    except KeyboardInterrupt:
        return 0
    finally:
        adapter.close()
        terminal.restore()


if __name__ == "__main__":
    raise SystemExit(main())
