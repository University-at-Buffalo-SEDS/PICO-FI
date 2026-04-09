#!/usr/bin/env python3
"""SPI-based test tool for Pico-Fi communication."""

import argparse
import time

try:
    from .raw import FRAME_SIZE, open_bus
except ImportError:
    import os
    import sys

    sys.path.append(os.path.dirname(__file__))
    from raw import FRAME_SIZE, open_bus

PAYLOAD_MAX = FRAME_SIZE - 2
REQ_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B
STALE_POLL_LIMIT = 4
COMMAND_POLL_LIMIT = 50
COMMAND_RETRY_LIMIT = 4
PROBE_PAUSE_S = 0.02


def build_frame(payload: bytes, magic: int = REQ_MAGIC) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2:2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, 0, b""
    if frame[0] == 0 and frame[1] in (RESP_MAGIC, RESP_COMMAND_MAGIC):
        magic_val = frame[1]
        length = frame[2]
        payload_start = 3
    else:
        magic_val = frame[0]
        length = frame[1]
        payload_start = 2
    if magic_val not in (RESP_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, 0, b""
    payload_end = payload_start + length
    if payload_end > FRAME_SIZE:
        return 0, 0, b""
    return magic_val, length, bytes(frame[payload_start:payload_end])


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def is_plausible_command_payload(payload: bytes) -> bool:
    if not payload:
        return True
    return all(byte in (9, 10, 13) or 32 <= byte <= 126 for byte in payload)


def flush_stale_command_frames(bus) -> None:
    for _ in range(STALE_POLL_LIMIT):
        frame = bus.read_frame()
        magic_val, length, body = parse_frame(frame)
        if magic_val == RESP_MAGIC and length == 0:
            return
        if magic_val == 0:
            return


def spi_exchange(
    bus_num: int,
    device: int,
    speed: int,
    payload: bytes,
    magic: int = REQ_MAGIC,
    require_valid_response: bool = True,
    verbose_raw: bool = False,
) -> int:
    bus = None
    try:
        bus = open_bus(bus_num, device, speed)
        tx = build_frame(payload, magic)
        print(f"Sending to SPI bus {bus_num}.{device} @ {speed} Hz...")
        print(f"Sent: {format_bytes(tx)}...")

        rx = bytes(FRAME_SIZE)
        magic_val, length, body = 0, 0, b""

        attempts = COMMAND_RETRY_LIMIT if magic == REQ_COMMAND_MAGIC else 1
        for attempt in range(attempts):
            if magic == REQ_COMMAND_MAGIC:
                flush_stale_command_frames(bus)
            first_rx = bus.write_frame(tx)
            first_magic, first_length, first_body = parse_frame(first_rx)
            if verbose_raw:
                print(f"Write recv[{attempt + 1}]: {format_bytes(first_rx)}...")

            rx = first_rx
            magic_val, length, body = first_magic, first_length, first_body
            if magic == REQ_COMMAND_MAGIC:
                if magic_val == RESP_COMMAND_MAGIC and is_plausible_command_payload(body):
                    break
                time.sleep(0.01)
                for _ in range(COMMAND_POLL_LIMIT):
                    rx = bus.read_frame()
                    magic_val, length, body = parse_frame(rx)
                    if magic_val == RESP_COMMAND_MAGIC and is_plausible_command_payload(body):
                        break
                    time.sleep(0.01)
                if magic_val == RESP_COMMAND_MAGIC and is_plausible_command_payload(body):
                    break
            elif magic_val == 0:
                rx = bus.read_frame()
                magic_val, length, body = parse_frame(rx)
                break
        print(f"Recv: {format_bytes(rx)}...")
        print(f"Magic: 0x{magic_val:02x}, Length: {length}")
        if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) and body:
            try:
                print(f"Response: {body.decode('utf-8', errors='replace')!r}")
            except Exception:
                print(f"Response: {body.hex()}")
        if require_valid_response:
            return 0 if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) else 1
        return 0
    except Exception as exc:
        print(f"ERROR: SPI error - {exc}")
        return 1
    finally:
        if bus is not None:
            try:
                bus.close()
            except Exception:
                pass


def spi_echo_test(bus_num: int, device: int, speed: int, payload: bytes) -> int:
    bus = None
    try:
        bus = open_bus(bus_num, device, speed)
        tx = build_frame(payload, REQ_COMMAND_MAGIC)
        print(f"Sending echo priming frame to SPI bus {bus_num}.{device} @ {speed} Hz...")
        print(f"Sent: {format_bytes(tx)}...")
        first = bus.write_frame(tx)
        print(f"Priming recv: {format_bytes(first)}...")
        second = bus.read_frame()
        print(f"Echo recv: {format_bytes(second)}...")
        if second == tx:
            print("Echo matched previous transaction exactly.")
            return 0
        print("Echo mismatch.")
        return 1
    except Exception as exc:
        print(f"ERROR: SPI error - {exc}")
        return 1
    finally:
        if bus is not None:
            try:
                bus.close()
            except Exception:
                pass


def main() -> int:
    parser = argparse.ArgumentParser(description="SPI test tool for Pico-Fi")
    parser.add_argument("--bus", type=int, default=0, help="SPI bus number")
    parser.add_argument("--device", type=int, default=0, help="SPI device number")
    parser.add_argument("--speed", type=int, default=100_000, help="SPI speed in Hz")
    parser.add_argument(
        "--verbose-raw",
        action="store_true",
        help="Print raw write-phase readback as well as the resolved response frame.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    probe_parser = subparsers.add_parser("probe", help="Probe with empty frames")
    probe_parser.add_argument("--count", type=int, default=10)
    cmd_parser = subparsers.add_parser("command", help="Send command")
    cmd_parser.add_argument("text", help="Command text (e.g., /ping)")
    echo_parser = subparsers.add_parser("echo", help="Send a frame and verify it is echoed back on the next transfer")
    echo_parser.add_argument(
        "text",
        nargs="?",
        default="/ping",
        help="Payload text to embed in the echoed frame",
    )
    led_parser = subparsers.add_parser("led", help="Send LED diagnostic commands")
    led_parser.add_argument(
        "action",
        choices=["on", "off", "toggle", "auto", "status"],
        help="LED action to request on the Pico",
    )
    led_parser.add_argument(
        "--fire-and-forget",
        action="store_true",
        help="Only verify the command was sent; do not require a valid response frame.",
    )
    args = parser.parse_args()
    if args.command == "probe":
        failures = 0
        for i in range(args.count):
            print(f"\n--- Probe {i + 1} ---")
            if spi_exchange(args.bus, args.device, args.speed, b"", verbose_raw=args.verbose_raw) != 0:
                failures += 1
            time.sleep(PROBE_PAUSE_S)
        print(f"\nProbe: {args.count - failures}/{args.count} successful")
        return 0 if failures == 0 else 1
    if args.command == "command":
        return spi_exchange(
            args.bus,
            args.device,
            args.speed,
            (args.text + "\n").encode(),
            REQ_COMMAND_MAGIC,
            verbose_raw=args.verbose_raw,
        )
    if args.command == "echo":
        return spi_echo_test(
            args.bus,
            args.device,
            args.speed,
            (args.text + "\n").encode(),
        )
    if args.command == "led":
        return spi_exchange(
            args.bus,
            args.device,
            args.speed,
            (f"/led {args.action}\n").encode(),
            REQ_COMMAND_MAGIC,
            require_valid_response=not args.fire_and_forget,
            verbose_raw=args.verbose_raw,
        )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
