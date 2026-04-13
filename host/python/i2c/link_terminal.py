#!/usr/bin/env python3
"""Interactive I2C terminal for Pico-Fi."""

from __future__ import annotations

import argparse
import collections
import queue
import shutil
import socket
import threading
import tty
from dataclasses import dataclass, field

import atexit
import select
import sys
import termios
import time

try:
    from .protocol import KIND_COMMAND, KIND_DATA, KIND_ERROR, read_packet, write_packet
    from .raw import open_bus
except ImportError:
    import os

    sys.path.append(os.path.dirname(__file__))
    from protocol import KIND_COMMAND, KIND_DATA, KIND_ERROR, read_packet, write_packet
    from raw import open_bus

I2C_ADDR = 0x55
CHUNK_DELAY_S = 0.001
INITIAL_RESPONSE_WAIT_S = 0.01


def print_payload(prompt: "PromptState", payload: bytes) -> None:
    text = payload.decode("utf-8", errors="replace").replace("\r", "")
    for line in text.split("\n"):
        if line:
            prompt.print_line(line)


def print_raw_packet(prompt: "PromptState", kind: int, payload: bytes) -> None:
    prompt.print_line(f"[raw] kind=0x{kind:02x} len={len(payload)} data={payload.hex(' ')}")


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


def format_outbound_chat(sender: str, line: str) -> str:
    return f"{sender}: {line}"


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
            line = prompt.handle_key(ch)
            if line is not None:
                outbound.put(line)
    except (EOFError, OSError):
        return


def exchange_packet(
        bus,
        prompt: PromptState,
        kind: int,
        payload: bytes,
        poll_delay_s: float,
        raw_mode: bool,
        transfer_id: int,
) -> None:
    try:
        write_packet(bus, I2C_ADDR, payload, kind=kind, transfer_id=transfer_id, chunk_delay_s=CHUNK_DELAY_S)
        time.sleep(INITIAL_RESPONSE_WAIT_S)
        response = read_packet(bus, I2C_ADDR, chunk_delay_s=CHUNK_DELAY_S, timeout_s=max(0.05, poll_delay_s))
        if response is None:
            return
        rx_kind, rx_payload = response
        if raw_mode:
            print_raw_packet(prompt, rx_kind, rx_payload)
        if rx_payload:
            print_payload(prompt, rx_payload)
        if rx_kind == KIND_ERROR:
            prompt.print_line("[pico] remote reported an i2c transport error")
    except Exception as exc:
        prompt.print_line(f"[error] I2C error: {exc}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Interactive I2C terminal for Pico-Fi")
    parser.add_argument("--bus", type=int, default=1)
    parser.add_argument("--addr", type=int, default=0x55)
    parser.add_argument("--poll-ms", type=int, default=50)
    parser.add_argument("--raw", action="store_true")
    parser.add_argument("--sender", default="")
    args = parser.parse_args()
    global I2C_ADDR
    I2C_ADDR = args.addr
    try:
        bus = open_bus(args.bus)
    except Exception as exc:
        print(f"ERROR: Cannot open I2C bus {args.bus}: {exc}")
        print("Make sure I2C is enabled and wired to GPIO0/GPIO1")
        return 1
    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)
    outbound: "queue.Queue[str]" = queue.Queue()
    terminal = TerminalModeGuard()
    terminal.enable()
    atexit.register(terminal.restore)
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()
    print(
        f"connected to I2C bus {args.bus} @ addr 0x{args.addr:02x}. "
        "plain text chats with the remote peer. / commands talk to the local Pico. "
        "//help for app help."
    )
    try:
        pending: collections.deque[tuple[int, bytes]] = collections.deque()
        transfer_id = 1
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
                        pending.append((KIND_COMMAND, (stripped + "\n").encode("utf-8")))
                    else:
                        rendered = format_outbound_chat(sender, line)
                        prompt.print_line(rendered)
                        pending.append((KIND_DATA, (rendered + "\n").encode("utf-8")))
            except queue.Empty:
                pass

            poll_delay_s = args.poll_ms / 1000.0
            if pending:
                kind, payload = pending.popleft()
                exchange_packet(bus, prompt, kind, payload, poll_delay_s, args.raw, transfer_id)
                transfer_id = (transfer_id + 1) & 0xFFFF or 1
            else:
                try:
                    response = read_packet(bus, I2C_ADDR, chunk_delay_s=CHUNK_DELAY_S, timeout_s=poll_delay_s)
                    if response is not None:
                        rx_kind, payload = response
                        if args.raw:
                            print_raw_packet(prompt, rx_kind, payload)
                        if payload:
                            print_payload(prompt, payload)
                except Exception:
                    pass
                time.sleep(poll_delay_s)
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
