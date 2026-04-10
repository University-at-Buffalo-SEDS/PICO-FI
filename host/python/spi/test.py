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
PULL_COMMAND = b"/pull\n"
STALE_POLL_LIMIT = 4
COMMAND_POLL_LIMIT = 50
COMMAND_RETRY_LIMIT = 4
DATA_POLL_LIMIT = 500
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
    for offset in range(0, min(2, FRAME_SIZE - 1)):
        magic_val = frame[offset]
        if magic_val not in (RESP_MAGIC, RESP_COMMAND_MAGIC):
            continue
        length = frame[offset + 1]
        if length > PAYLOAD_MAX:
            continue
        payload_start = offset + 2
        payload_end = payload_start + length
        if payload_end > FRAME_SIZE:
            continue
        return magic_val, length, bytes(frame[payload_start:payload_end])
    return 0, 0, b""


def format_bytes(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data[:16])


def is_plausible_command_payload(payload: bytes) -> bool:
    if not payload:
        return False
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
    expect_command_reply: bool = False,
    await_nonempty_data: bool = False,
    expected_text: str | None = None,
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

        attempts = COMMAND_RETRY_LIMIT if expect_command_reply else 1
        for attempt in range(attempts):
            first_rx = bus.write_frame(tx)
            first_magic, first_length, first_body = parse_frame(first_rx)
            if verbose_raw:
                print(f"Write recv[{attempt + 1}]: {format_bytes(first_rx)}...")

            rx = first_rx
            magic_val, length, body = first_magic, first_length, first_body
            if expect_command_reply:
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
            if await_nonempty_data and not (magic_val == RESP_MAGIC and body):
                for _ in range(DATA_POLL_LIMIT):
                    time.sleep(0.01)
                    rx = bus.read_frame()
                    magic_val, length, body = parse_frame(rx)
                    if magic_val == RESP_MAGIC and body:
                        break
            break
        print(f"Recv: {format_bytes(rx)}...")
        print(f"Magic: 0x{magic_val:02x}, Length: {length}")
        if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) and body:
            try:
                print(f"Response: {body.decode('utf-8', errors='replace')!r}")
            except Exception:
                print(f"Response: {body.hex()}")
        if expected_text is not None:
            actual = body.decode("utf-8", errors="replace")
            if expected_text not in actual:
                print(
                    f"ERROR: Expected response containing {expected_text!r}, got {actual!r}"
                )
                return 1
        if require_valid_response:
            if expect_command_reply:
                return 0 if magic_val == RESP_COMMAND_MAGIC and is_plausible_command_payload(body) else 1
            if await_nonempty_data:
                return 0 if magic_val == RESP_MAGIC and bool(body) else 1
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
        for poll in range(4):
            time.sleep(0.01)
            second = bus.read_frame()
            print(f"Echo recv[{poll + 1}]: {format_bytes(second)}...")
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


def spi_recv_via_pull(
    bus_num: int,
    device: int,
    speed: int,
    expected_text: str | None = None,
    verbose_raw: bool = False,
) -> int:
    bus = None
    try:
        bus = open_bus(bus_num, device, speed)
        tx = build_frame(PULL_COMMAND, REQ_COMMAND_MAGIC)
        print(f"Sending to SPI bus {bus_num}.{device} @ {speed} Hz...")
        print(f"Sent: {format_bytes(tx)}...")

        rx = bytes(FRAME_SIZE)
        magic_val, length, body = 0, 0, b""
        matched = False
        for pull_attempt in range(DATA_POLL_LIMIT):
            if pull_attempt:
                time.sleep(0.01)
            rx = bus.write_frame(tx)
            if verbose_raw:
                print(f"Write recv[{pull_attempt + 1}]: {format_bytes(rx)}...")
            magic_val, length, body = parse_frame(rx)
            if magic_val == RESP_MAGIC and body:
                if expected_text is None or expected_text in body.decode("utf-8", errors="replace"):
                    matched = True
                    break
            for poll_attempt in range(COMMAND_POLL_LIMIT):
                time.sleep(0.01)
                rx = bus.read_frame()
                if verbose_raw:
                    poll_index = pull_attempt * COMMAND_POLL_LIMIT + poll_attempt + 1
                    print(f"Poll recv[{poll_index}]: {format_bytes(rx)}...")
                magic_val, length, body = parse_frame(rx)
                if magic_val == RESP_MAGIC and body:
                    if expected_text is None or expected_text in body.decode("utf-8", errors="replace"):
                        matched = True
                        break
                    break
            if matched:
                break

        print(f"Recv: {format_bytes(rx)}...")
        print(f"Magic: 0x{magic_val:02x}, Length: {length}")
        if magic_val in (RESP_MAGIC, RESP_COMMAND_MAGIC) and body:
            print(f"Response: {body.decode('utf-8', errors='replace')!r}")
        if expected_text is not None:
            actual = body.decode("utf-8", errors="replace")
            if expected_text not in actual:
                print(
                    f"ERROR: Expected response containing {expected_text!r}, got {actual!r}"
                )
                return 1
        return 0 if magic_val == RESP_MAGIC and body and (expected_text is None or matched) else 1
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
    data_parser = subparsers.add_parser("data", help="Send framed data and await a non-empty data reply")
    data_parser.add_argument("text")
    data_parser.add_argument(
        "--expect",
        default="",
        help="Substring expected in the returned data payload.",
    )
    send_parser = subparsers.add_parser("send", help="Send framed data and only require a valid immediate response")
    send_parser.add_argument("text")
    recv_parser = subparsers.add_parser("recv", help="Poll with empty framed data frames until non-empty data is returned")
    recv_parser.add_argument(
        "--expect",
        default="",
        help="Substring expected in the returned data payload.",
    )
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
            expect_command_reply=True,
            verbose_raw=args.verbose_raw,
        )
    if args.command == "data":
        return spi_exchange(
            args.bus,
            args.device,
            args.speed,
            args.text.encode(),
            REQ_MAGIC,
            await_nonempty_data=True,
            expected_text=args.expect or None,
            verbose_raw=args.verbose_raw,
        )
    if args.command == "send":
        return spi_exchange(
            args.bus,
            args.device,
            args.speed,
            args.text.encode(),
            REQ_MAGIC,
            verbose_raw=args.verbose_raw,
        )
    if args.command == "recv":
        return spi_recv_via_pull(
            args.bus,
            args.device,
            args.speed,
            expected_text=args.expect or None,
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
            expect_command_reply=not args.fire_and_forget,
            verbose_raw=args.verbose_raw,
        )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
