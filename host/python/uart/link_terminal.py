#!/usr/bin/env python3
"""Interactive framed UART terminal for Pico-Fi."""

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

import serial

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_DATA_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def open_serial(port: str, baud: int) -> serial.Serial:
    return serial.Serial(
        port=port,
        baudrate=baud,
        timeout=0.1,
        bytesize=serial.EIGHTBITS,
        parity=serial.PARITY_NONE,
        stopbits=serial.STOPBITS_ONE,
        xonxoff=False,
        rtscts=False,
        dsrdtr=False,
    )


def build_frame(payload: bytes, magic: int) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, b""
    magic = frame[0]
    length = frame[1]
    if magic not in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, b""
    return magic, bytes(frame[2 : 2 + length])


def read_frame(ser: serial.Serial, timeout_s: float) -> bytes | None:
    deadline = time.monotonic() + timeout_s
    buf = bytearray()
    while time.monotonic() < deadline and len(buf) < FRAME_SIZE:
        chunk = ser.read(FRAME_SIZE - len(buf))
        if chunk:
            buf.extend(chunk)
    if len(buf) == FRAME_SIZE:
        return bytes(buf)
    return None


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


def print_payload(prompt: PromptState, payload: bytes) -> None:
    text = payload.decode("utf-8", errors="replace").replace("\r", "")
    for line in text.split("\n"):
        if line:
            prompt.print_line(line)


def send_frame(ser: serial.Serial, magic: int, payload: bytes) -> None:
    ser.write(build_frame(payload, magic))
    ser.flush()


def exchange_command(ser: serial.Serial, prompt: PromptState, command: str) -> None:
    send_frame(ser, REQ_COMMAND_MAGIC, (command + "\n").encode("utf-8"))
    frame = read_frame(ser, 2.0)
    if frame is None:
        prompt.print_line("[pico] command timed out waiting for UART reply")
        return
    magic, payload = parse_frame(frame)
    if magic != RESP_COMMAND_MAGIC:
        prompt.print_line("[pico] invalid UART command reply")
        return
    if payload:
        print_payload(prompt, payload)


def main() -> int:
    parser = argparse.ArgumentParser(description="Interactive framed UART terminal for Pico-Fi")
    parser.add_argument("--port", required=True)
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--sender", default="")
    parser.add_argument("--poll-ms", type=int, default=100)
    args = parser.parse_args()
    try:
        ser = open_serial(args.port, args.baud)
    except serial.SerialException as exc:
        print(f"error: failed to open {args.port}: {exc}", file=sys.stderr)
        return 1

    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)
    outbound: "queue.Queue[str]" = queue.Queue()
    terminal = TerminalModeGuard()
    terminal.enable()
    atexit.register(terminal.restore)
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()

    print(
        f"connected to UART {args.port} @ {args.baud}. "
        "plain text chats with the remote peer. / commands talk to the local Pico. "
        "//help for app help."
    )

    try:
        with ser:
            ser.reset_input_buffer()
            ser.reset_output_buffer()
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
                        if stripped.startswith("/") and not stripped.startswith("//"):
                            prompt.print_line(f"[pico] {stripped}")
                            exchange_command(ser, prompt, stripped)
                        else:
                            prompt.print_line(f"[{sender}] {line}")
                            send_frame(
                                ser,
                                REQ_DATA_MAGIC,
                                f"[{sender}] {line}\n".encode("utf-8"),
                            )
                except queue.Empty:
                    pass

                frame = read_frame(ser, args.poll_ms / 1000.0)
                if frame is None:
                    continue

                magic, payload = parse_frame(frame)
                if magic in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) and payload:
                    print_payload(prompt, payload)
    except KeyboardInterrupt:
        return 0
    finally:
        terminal.restore()


if __name__ == "__main__":
    raise SystemExit(main())
