#!/usr/bin/env python3

from __future__ import annotations

import argparse
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
    import spidev
except ImportError as exc:  # pragma: no cover - runtime dependency
    raise SystemExit(
        "error: spidev is required. Install it with `python3 -m pip install spidev`."
    ) from exc


FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> list[int]:
    payload = payload[:PAYLOAD_MAX]
    frame = [0] * FRAME_SIZE
    frame[0] = magic
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return frame


def parse_frame(frame: list[int]) -> tuple[int, bytes]:
    if len(frame) != FRAME_SIZE or frame[0] not in (RESP_MAGIC, RESP_COMMAND_MAGIC):
        return 0, b""
    length = min(frame[1], PAYLOAD_MAX)
    return frame[0], bytes(frame[2 : 2 + length])


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


def input_loop(outbound: "queue.Queue[str]", prompt: PromptState) -> None:
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    try:
        tty.setcbreak(fd)
        prompt.redraw()
        while True:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if not ready:
                continue
            ch = sys.stdin.read(1)
            line = prompt.handle_key(ch)
            if line is not None:
                outbound.put(line)
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


def main() -> int:
    parser = argparse.ArgumentParser(description="Interactive SPI master terminal for the Pico SPI-slave bridge.")
    parser.add_argument("--bus", type=int, default=0)
    parser.add_argument("--device", type=int, default=0)
    parser.add_argument("--speed", type=int, default=50_000)
    parser.add_argument("--mode", type=int, default=0)
    parser.add_argument("--poll-ms", type=int, default=50)
    parser.add_argument("--sender", default="", help="Sender label prepended to outbound chat lines.")
    args = parser.parse_args()

    spi = spidev.SpiDev()
    spi.open(args.bus, args.device)
    spi.max_speed_hz = args.speed
    spi.mode = args.mode
    spi.bits_per_word = 8

    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)
    outbound: "queue.Queue[str]" = queue.Queue()
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()

    print(
        f"connected to /dev/spidev{args.bus}.{args.device} @ {args.speed}Hz mode{args.mode}. "
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
                        return 0
                    if not line:
                        continue
                    prompt.print_line(f"[{sender}] {line}")
                    if stripped.startswith("/") and not stripped.startswith("//"):
                        pending.append((REQ_COMMAND_MAGIC, (stripped + "\n").encode("utf-8")))
                    else:
                        pending.append((REQ_MAGIC, f"[{sender}] {line}\n".encode("utf-8")))
            except queue.Empty:
                pass

            if pending:
                magic, payload = pending.popleft()
                tx = build_frame(payload, magic)
            else:
                tx = build_frame(b"")
            rx = spi.xfer2(tx)
            _, payload = parse_frame(rx)
            if payload:
                text = payload.decode("utf-8", errors="replace").replace("\r", "")
                for line in text.split("\n"):
                    if line:
                        prompt.print_line(line)
            time.sleep(args.poll_ms / 1000.0)
    except KeyboardInterrupt:
        return 0
    finally:
        spi.close()


if __name__ == "__main__":
    raise SystemExit(main())
