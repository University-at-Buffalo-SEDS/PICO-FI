#!/usr/bin/env python3

from __future__ import annotations

import argparse
import select
import socket
import sys
import termios
import threading
import time
import tty
from dataclasses import dataclass, field

try:
    import serial
except ImportError as exc:  # pragma: no cover - runtime dependency
    raise SystemExit(
        "error: pyserial is required. Install it with `python3 -m pip install pyserial`."
    ) from exc


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

    def redraw(self) -> None:
        sys.stdout.write("\r\033[2K" + self.prompt + self.buffer)
        sys.stdout.flush()

    def print_line(self, line: str) -> None:
        with self.lock:
            sys.stdout.write("\r\033[2K" + line + "\n")
            self.redraw()

    def replace_buffer(self, value: str) -> None:
        with self.lock:
            self.buffer = value
            self.redraw()

    def handle_key(self, ch: str) -> str | None:
        with self.lock:
            if ch in ("\r", "\n"):
                line = self.buffer
                self.buffer = ""
                sys.stdout.write("\r\033[2K")
                sys.stdout.flush()
                return line
            if ch in ("\x7f", "\b"):
                self.buffer = self.buffer[:-1]
            elif ch.isprintable():
                self.buffer += ch
            self.redraw()
            return None


def read_loop(ser: serial.Serial, prompt: PromptState) -> None:
    partial = ""
    while True:
        try:
            data = ser.read(256)
        except serial.SerialException as exc:
            prompt.print_line(f"[serial read error] {exc}")
            return
        if not data:
            continue
        partial += data.decode("utf-8", errors="replace")
        while "\n" in partial:
            line, partial = partial.split("\n", 1)
            prompt.print_line(line.rstrip("\r"))


def send_line(ser: serial.Serial, sender: str, line: str) -> None:
    stripped = line.strip()
    if stripped.startswith("/") and not stripped.startswith("//"):
        payload = stripped + "\n"
    else:
        payload = f"[{sender}] {line}\n"
    ser.write(payload.encode("utf-8"))
    ser.flush()


def write_loop(ser: serial.Serial, prompt: PromptState) -> int:
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
            if line is None:
                continue
            stripped = line.strip()
            if stripped == "//help":
                for help_line in print_help():
                    prompt.print_line(help_line)
                continue
            if stripped == "//quit":
                return 0
            if not line:
                prompt.redraw()
                continue
            prompt.print_line(f"[{prompt.sender}] {line}")
            send_line(ser, prompt.sender, line)
    except KeyboardInterrupt:
        return 0
    except serial.SerialException as exc:
        prompt.print_line(f"[serial write error] {exc}")
        return 1
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Interactive UART terminal for the two-Pico network bridge."
    )
    parser.add_argument("--port", required=True, help="Serial device, e.g. /dev/cu.usbmodemXXXX or /dev/serial0")
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--label", default="", help="Optional startup label.")
    parser.add_argument("--sender", default="", help="Sender label prepended to outbound chat lines.")
    args = parser.parse_args()

    try:
        ser = serial.Serial(args.port, args.baud, timeout=0.1)
    except serial.SerialException as exc:
        print(f"error: failed to open {args.port}: {exc}", file=sys.stderr)
        return 1

    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)

    with ser:
        if args.label:
            print(f"[{args.label}] connected to {args.port} @ {args.baud}")
        else:
            print(f"connected to {args.port} @ {args.baud}")
        print("plain text chats with the remote peer. / commands talk to the local Pico. //help for app help.")

        reader = threading.Thread(target=read_loop, args=(ser, prompt), daemon=True)
        reader.start()
        status = write_loop(ser, prompt)
        time.sleep(0.1)
        return status


if __name__ == "__main__":
    raise SystemExit(main())
