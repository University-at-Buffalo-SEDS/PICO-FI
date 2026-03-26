#!/usr/bin/env python3
"""UART-based test tool for Pico-Fi communication."""

import argparse
import sys
import time
import serial

try:
    import spidev
except ImportError:
    spidev = None

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    """Build a frame for sending"""
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2+len(payload)] = payload
    return bytes(frame)


def is_garbage_frame(frame: bytes) -> bool:
    """Detect if frame is garbage (mostly 0xFF)"""
    ff_count = sum(1 for b in frame if b == 0xFF)
    return ff_count > len(frame) * 0.8


def parse_frame(frame: bytes) -> tuple[int, int, bytes]:
    """Extract data from frame"""
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    
    if is_garbage_frame(frame):
        return 0, 0, b""
    
    response_bytes = bytearray()
    for byte_val in frame:
        if byte_val == 0:
            break
        response_bytes.append(byte_val)
    
    return 0, len(response_bytes), bytes(response_bytes)


def format_bytes(data: bytes) -> str:
    """Format bytes for display"""
    return " ".join(f"{b:02x}" for b in data[:16])


def uart_exchange(port: str, speed: int, payload: bytes, magic: int = REQ_MAGIC) -> int:
    """Send frame via UART and receive response"""
    try:
        with serial.Serial(port, speed, timeout=2) as ser:
            tx = build_frame(payload, magic)
            
            # Send frame
            ser.write(tx)
            print(f"Sent: {format_bytes(tx)}...")
            
            # Receive response
            rx = ser.read(FRAME_SIZE)
            if len(rx) < FRAME_SIZE:
                print(f"ERROR: Incomplete response ({len(rx)} bytes)")
                return 1
            
            print(f"Recv: {format_bytes(bytes(rx))}...")
            
            # Parse
            magic_val, length, body = parse_frame(rx)
            valid = magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC)
            
            if valid and body:
                try:
                    decoded = body.decode('utf-8', errors='replace')
                    print(f"Response: {decoded!r}")
                except:
                    print(f"Response: {body.hex()}")
            
            return 0 if valid else 1
    
    except serial.SerialException as e:
        print(f"ERROR: Serial error - {e}")
        print(f"Make sure device is connected to {port}")
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description="UART test tool for Pico-Fi")
    parser.add_argument("--port", default="/dev/ttyAMA0", help="Serial port")
    parser.add_argument("--speed", type=int, default=115200, help="Baud rate")
    
    subparsers = parser.add_subparsers(dest="command", required=True)
    
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
    probe_parser.add_argument("--count", type=int, default=10)
    
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text", help="Command text (e.g., /ping)")
    
    args = parser.parse_args()
    
    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            result = uart_exchange(args.port, args.speed, b"")
            if result != 0:
                failures += 1
            time.sleep(0.1)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    
    elif args.command == "command":
        text = args.text + "\n"
        return uart_exchange(args.port, args.speed, text.encode(), REQ_COMMAND_MAGIC)
    
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
