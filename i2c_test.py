#!/usr/bin/env python3
"""
I2C-based test tool for Pico-Fi communication
Uses raw Linux I2C_RDWR transfers to communicate with Pico
"""

import argparse
import time

from i2c_raw import CHUNK_SIZE, open_bus

FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
I2C_ADDR = 0x55  # Pico I2C slave address

REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
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

    magic_val = frame[0]
    length = frame[1]
    if magic_val not in (RESP_MAGIC, RESP_COMMAND_MAGIC):
        return 0, 0, b""
    if length > PAYLOAD_MAX:
        return 0, 0, b""
    body = bytes(frame[2:2 + length])
    return magic_val, length, body


def format_bytes(data: bytes) -> str:
    """Format bytes for display"""
    return " ".join(f"{b:02x}" for b in data[:16])


def i2c_exchange(bus_num: int, payload: bytes, magic: int = REQ_MAGIC) -> int:
    """Send frame via I2C and receive response"""
    try:
        bus = open_bus(bus_num)
        tx = build_frame(payload, magic)
        
        print(f"Sending to I2C addr 0x{I2C_ADDR:02x}...")
        print(f"Sent: {format_bytes(tx)}...")
        
        for i in range(0, FRAME_SIZE, CHUNK_SIZE):
            chunk = tx[i:i + CHUNK_SIZE]
            bus.write(I2C_ADDR, chunk)
            time.sleep(0.01)
        
        time.sleep(0.1)
        
        rx = bytearray()
        for i in range(0, FRAME_SIZE, CHUNK_SIZE):
            chunk_size = min(CHUNK_SIZE, FRAME_SIZE - len(rx))
            chunk = bus.read(I2C_ADDR, chunk_size)
            rx.extend(chunk)
            time.sleep(0.01)
        
        rx = bytes(rx[:FRAME_SIZE])
        bus.close()
        
        print(f"Recv: {format_bytes(rx)}...")
        
        # Parse
        magic_val, length, body = parse_frame(rx)
        valid = magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC)
        
        print(f"Magic: 0x{magic_val:02x}, Length: {length}")
        
        if valid and body:
            try:
                decoded = body.decode('utf-8', errors='replace')
                print(f"Response: {decoded!r}")
            except:
                print(f"Response: {body.hex()}")
        
        return 0 if valid else 1
    
    except Exception as e:
        print(f"ERROR: I2C error - {e}")
        try:
            bus.close()
        except:
            pass
        return 1


def main() -> int:
    parser = argparse.ArgumentParser(description="I2C test tool for Pico-Fi")
    parser.add_argument("--bus", type=int, default=1, help="I2C bus number")
    parser.add_argument("--addr", type=int, default=0x55, help="Pico I2C address (hex)")
    
    subparsers = parser.add_subparsers(dest="command", required=True)
    
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
    probe_parser.add_argument("--count", type=int, default=10)
    
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text", help="Command text (e.g., /ping)")
    
    args = parser.parse_args()
    global I2C_ADDR
    I2C_ADDR = args.addr
    
    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            print(f"\n--- Probe {i+1} ---")
            result = i2c_exchange(args.bus, b"")
            if result != 0:
                failures += 1
            time.sleep(0.2)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    
    elif args.command == "command":
        text = args.text + "\n"
        return i2c_exchange(args.bus, text.encode(), REQ_COMMAND_MAGIC)
    
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
