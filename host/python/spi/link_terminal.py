#!/usr/bin/env python3
"""Interactive SPI terminal for Pico-Fi."""

from __future__ import annotations

import argparse
import atexit
import collections
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
    from .raw import FRAME_SIZE, open_bus
except ImportError:
    import os

    sys.path.append(os.path.dirname(__file__))
    from raw import FRAME_SIZE, open_bus

PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B
COMMAND_POLL_LIMIT = 50
COMMAND_RETRY_LIMIT = 4


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, b""
    if frame[0] == 0 and frame[1] in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC):
        magic = frame[1]
        length = frame[2]
        payload_start = 3
    else:
        magic = frame[0]
        length = frame[1]
        payload_start = 2
    if magic not in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, b""
    payload_end = payload_start + length
    if payload_end > FRAME_SIZE:
        return 0, b""
    return magic, bytes(frame[payload_start:payload_end])


def print_help() -> list[str]:
    return [
        "chat mode:",
        "  plain text   send to the remote peer with sender label",
        "  /help        ask the local Pico for command help",
        "  /show        show the local Pico config",
        "  /ping        ping the local Pico",
        "  /link        show the local Pico link state",
        "  //help       show this app help",
        "  //quit       exit the app",
    ]


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
        return "local"


@dataclass
class PromptState:
    sender: str
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


@dataclass
class StreamPrinter:
    prompt: PromptState
    pending: str = ""

    def feed(self, payload: bytes) -> None:
        text = payload.decode("utf-8", errors="replace").replace("\r", "")
        if not text:
            return
        self.pending += text
        while True:
            newline = self.pending.find("\n")
            if newline < 0:
                break
            line = self.pending[:newline]
            self.pending = self.pending[newline + 1 :]
            if line:
                self.prompt.print_line(line)

    def flush_partial(self) -> None:
        if self.pending:
            self.prompt.print_line(self.pending)
            self.pending = ""


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


def is_plausible_command_payload(payload: bytes) -> bool:
    if not payload:
        return False
    return all(byte in (9, 10, 13) or 32 <= byte <= 126 for byte in payload)


def exchange_frame(
    bus,
    prompt: PromptState,
    stream_printer: StreamPrinter,
    magic: int,
    payload: bytes,
    poll_delay_s: float,
) -> None:
    try:
        if magic != REQ_COMMAND_MAGIC:
            first_rx = bus.write_frame(build_frame(payload, magic))
            rx_magic, rx_payload = parse_frame(first_rx)
            if rx_magic == 0:
                rx_magic, rx_payload = parse_frame(bus.read_frame())
            if rx_magic in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) and rx_payload:
                stream_printer.feed(rx_payload)
                stream_printer.flush_partial()
            return

        last_magic = 0
        last_payload = b""
        for _ in range(COMMAND_RETRY_LIMIT):
            first_rx = bus.write_frame(build_frame(payload, magic))
            rx_magic, rx_payload = parse_frame(first_rx)
            last_magic, last_payload = rx_magic, rx_payload
            if rx_magic == RESP_DATA_MAGIC and rx_payload:
                stream_printer.feed(rx_payload)
                stream_printer.flush_partial()
            if rx_magic == RESP_COMMAND_MAGIC and is_plausible_command_payload(rx_payload):
                stream_printer.feed(rx_payload)
                stream_printer.flush_partial()
                return
            time.sleep(poll_delay_s)
            for _ in range(COMMAND_POLL_LIMIT):
                rx_magic, rx_payload = parse_frame(bus.read_frame())
                last_magic, last_payload = rx_magic, rx_payload
                if rx_magic == RESP_DATA_MAGIC and rx_payload:
                    stream_printer.feed(rx_payload)
                    stream_printer.flush_partial()
                if rx_magic == RESP_COMMAND_MAGIC and is_plausible_command_payload(rx_payload):
                    stream_printer.feed(rx_payload)
                    stream_printer.flush_partial()
                    return
                time.sleep(poll_delay_s)
        if last_magic == RESP_COMMAND_MAGIC and last_payload:
            stream_printer.feed(last_payload)
            stream_printer.flush_partial()
            return
        prompt.print_line("[pico] command timed out waiting for SPI reply")
    except Exception as exc:
        prompt.print_line(f"[error] SPI error: {exc}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Interactive SPI terminal for Pico-Fi")
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=100_000)
    parser.add_argument("--poll-ms", type=int, default=50)
    parser.add_argument("--sender", default="")
    args = parser.parse_args()
    try:
        bus = open_bus(args.bus, args.device, args.speed)
    except Exception as exc:
        print(f"ERROR: Cannot open SPI bus {args.bus}.{args.device}: {exc}")
        print("Make sure SPI is enabled and wired to GPIO10/11/12/13")
        return 1
    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)
    stream_printer = StreamPrinter(prompt=prompt)
    outbound: "queue.Queue[str]" = queue.Queue()
    terminal = TerminalModeGuard()
    terminal.enable()
    atexit.register(terminal.restore)
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()
    print(
        f"connected to SPI bus {args.bus}.{args.device} @ {args.speed} Hz. "
        "plain text chats with the remote peer. / commands talk to the local Pico. "
        "//help for app help."
    )
    try:
        pending: collections.deque[tuple[int, bytes]] = collections.deque()
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
                        bus.close()
                        return 0
                    if not line:
                        continue
                    if stripped.startswith("/") and not stripped.startswith("//"):
                        prompt.print_line(f"[pico] {stripped}")
                        pending.append((REQ_COMMAND_MAGIC, (stripped + "\n").encode("utf-8")))
                    else:
                        prompt.print_line(f"[{sender}] {line}")
                        pending.append((REQ_COMMAND_MAGIC, f"[{sender}] {line}\n".encode("utf-8")))
            except queue.Empty:
                pass

            poll_delay_s = args.poll_ms / 1000.0
            if pending:
                magic, payload = pending.popleft()
                exchange_frame(bus, prompt, stream_printer, magic, payload, poll_delay_s)
            else:
                try:
                    rx_magic, payload = parse_frame(bus.read_frame())
                    if rx_magic == RESP_DATA_MAGIC and payload:
                        stream_printer.feed(payload)
                except Exception:
                    pass
                time.sleep(max(poll_delay_s, 0.1))
    except KeyboardInterrupt:
        terminal.restore()
        return 0
    finally:
        terminal.restore()
        try:
            bus.close()
        except Exception:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
