#!/usr/bin/env python3
"""
I2C interactive terminal for Pico-Fi
Real-time bidirectional communication over I2C
"""

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
    from i2c_raw import CHUNK_SIZE, open_bus
except ImportError:
    raise SystemExit("error: raw I2C helpers unavailable.")

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
I2C_ADDR = 0x55
CHUNK_DELAY_S = 0.001
INITIAL_RESPONSE_WAIT_S = 0.01

REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    """Build I2C frame"""
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(2 + len(payload))
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2+len(payload)] = payload
    return bytes(frame)


def is_garbage_frame(frame: bytes) -> bool:
    """Detect garbage frame"""
    ff_count = sum(1 for b in frame if b == 0xFF)
    return ff_count > len(frame) * 0.8


def parse_frame(frame: bytes) -> tuple[int, bytes]:
    """Extract response from frame"""
    if len(frame) != FRAME_SIZE:
        return 0, b""
    if is_garbage_frame(frame):
        return 0, b""

    magic = frame[0]
    length = frame[1]
    if magic not in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC):
        return 0, b""
    if length > PAYLOAD_MAX:
        return 0, b""
    return magic, bytes(frame[2:2 + length])


def print_payload(prompt: "PromptState", payload: bytes) -> None:
    """Print response payload"""
    text = payload.decode("utf-8", errors="replace").replace("\r", "")
    for line in text.split("\n"):
        if line:
            prompt.print_line(line)


def print_raw_frame(prompt: "PromptState", magic: int, payload: bytes) -> None:
    """Print a compact raw frame summary"""
    hex_payload = payload.hex(" ")
    prompt.print_line(f"[raw] magic=0x{magic:02x} len={len(payload)} data={hex_payload}")


def read_frame(bus) -> bytes:
    """Read one full framed response from the Pico"""
    rx = bytearray()
    for _ in range(0, FRAME_SIZE, CHUNK_SIZE):
        chunk_size = min(CHUNK_SIZE, FRAME_SIZE - len(rx))
        chunk = bus.read(I2C_ADDR, chunk_size)
        rx.extend(chunk)
        if len(rx) < FRAME_SIZE:
            time.sleep(CHUNK_DELAY_S)
    return bytes(rx[:FRAME_SIZE])


def write_frame(bus, frame: bytes) -> None:
    """Write one full framed request to the Pico"""
    for i in range(0, len(frame), CHUNK_SIZE):
        chunk = frame[i:i + CHUNK_SIZE]
        bus.write(I2C_ADDR, chunk)
        if i + CHUNK_SIZE < len(frame):
            time.sleep(CHUNK_DELAY_S)


def exchange_frame(
    bus,
    prompt: "PromptState",
    magic: int,
    payload: bytes,
    poll_delay_s: float,
    raw_mode: bool,
    command_timeout_polls: int = 50,
) -> None:
    """Send frame and collect response"""
    try:
        tx = build_frame(payload, magic)

        write_frame(bus, tx)
        time.sleep(INITIAL_RESPONSE_WAIT_S)

        rx = read_frame(bus)
        rx_magic, rx_payload = parse_frame(rx)
        if raw_mode and rx_magic:
            print_raw_frame(prompt, rx_magic, rx_payload)
        
        if magic != REQ_COMMAND_MAGIC:
            if rx_payload:
                print_payload(prompt, rx_payload)
            return

        if rx_magic == RESP_COMMAND_MAGIC and rx_payload:
            print_payload(prompt, rx_payload)
            return

        for _ in range(command_timeout_polls):
            time.sleep(poll_delay_s)
            rx = read_frame(bus)
            rx_magic, rx_payload = parse_frame(rx)
            if raw_mode and rx_magic:
                print_raw_frame(prompt, rx_magic, rx_payload)

            if rx_magic == RESP_COMMAND_MAGIC and rx_payload:
                print_payload(prompt, rx_payload)
                return

        prompt.print_line("[pico] command timed out waiting for I2C reply")
    
    except Exception as e:
        prompt.print_line(f"[error] I2C error: {e}")


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
    """Handle terminal input"""
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
    parser = argparse.ArgumentParser(description="Interactive I2C terminal for Pico-Fi")
    parser.add_argument("--bus", type=int, default=1, help="I2C bus number")
    parser.add_argument("--addr", type=int, default=0x55, help="Pico I2C address")
    parser.add_argument("--poll-ms", type=int, default=50, help="Poll delay in ms")
    parser.add_argument("--raw", action="store_true", help="Print raw framed I2C responses")
    parser.add_argument("--sender", default="", help="Sender label prepended to chat lines")
    args = parser.parse_args()
    global I2C_ADDR
    I2C_ADDR = args.addr

    try:
        bus = open_bus(args.bus)
    except Exception as e:
        print(f"ERROR: Cannot open I2C bus {args.bus}: {e}")
        print("Make sure:")
        print(f"  1. I2C{args.bus} is enabled")
        print("  2. Pico is connected (GPIO0↔SDA, GPIO1↔SCL)")
        print("  3. Running with sudo (if needed)")
        return 1

    sender = args.sender or default_sender_label()
    prompt = PromptState(sender=sender)
    outbound: "queue.Queue[str]" = queue.Queue()
    threading.Thread(target=input_loop, args=(outbound, prompt), daemon=True).start()

    print(
        f"connected to I2C bus {args.bus} @ addr 0x{args.addr:02x}. "
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
                    prompt.print_line(f"[{sender}] {line}")
                    if stripped.startswith("/") and not stripped.startswith("//"):
                        pending.append((REQ_COMMAND_MAGIC, (stripped + "\n").encode("utf-8")))
                    else:
                        pending.append((REQ_MAGIC, f"[{sender}] {line}\n".encode("utf-8")))
            except queue.Empty:
                pass

            poll_delay_s = args.poll_ms / 1000.0
            if pending:
                magic, payload = pending.popleft()
                exchange_frame(bus, prompt, magic, payload, poll_delay_s, args.raw)
            else:
                # Poll for incoming data
                try:
                    rx = read_frame(bus)
                    rx_magic, payload = parse_frame(rx)
                    if args.raw and rx_magic:
                        print_raw_frame(prompt, rx_magic, payload)
                    if rx_magic == RESP_DATA_MAGIC and payload:
                        print_payload(prompt, payload)
                except Exception:
                    pass
                
                time.sleep(poll_delay_s)
    except KeyboardInterrupt:
        bus.close()
        return 0
    finally:
        try:
            bus.close()
        except:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
