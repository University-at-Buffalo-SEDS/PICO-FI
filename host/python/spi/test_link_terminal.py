#!/usr/bin/env python3
"""Regression tests for the interactive SPI terminal timing behavior."""

from __future__ import annotations

import sys
import types
import unittest
from unittest import mock

if "spidev" not in sys.modules:
    sys.modules["spidev"] = types.SimpleNamespace(SpiDev=object)

from host.python.spi import link_terminal as lt


def frame(magic: int, payload: bytes = b"") -> bytes:
    return lt.build_frame(payload, magic)


class FakePrompt:
    def __init__(self) -> None:
        self.lines: list[str] = []

    def print_line(self, line: str) -> None:
        self.lines.append(line)


class FakeStreamPrinter:
    def __init__(self) -> None:
        self.payloads: list[bytes] = []
        self.flushed = 0

    def feed(self, payload: bytes) -> None:
        self.payloads.append(payload)

    def flush_partial(self) -> None:
        self.flushed += 1


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def monotonic(self) -> float:
        return self.now

    def sleep(self, duration: float) -> None:
        self.now += max(duration, 0.0)


class TimingSensitiveBus:
    def __init__(self, clock: FakeClock, min_gap_s: float) -> None:
        self.clock = clock
        self.min_gap_s = min_gap_s
        self.last_close_s: float | None = None
        self.last_command_raw: bytes | None = None

    def __enter__(self) -> "TimingSensitiveBus":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.last_close_s = self.clock.monotonic()

    def _stable(self) -> bool:
        if self.last_close_s is None:
            return True
        return (self.clock.monotonic() - self.last_close_s) >= self.min_gap_s

    def write_frame(self, raw: bytes) -> bytes:
        self.last_command_raw = raw
        if not self._stable():
            return bytes(lt.FRAME_SIZE)
        magic = raw[0]
        payload_len = raw[1]
        payload = bytes(raw[2 : 2 + payload_len])
        if magic == lt.REQ_COMMAND_MAGIC and payload.startswith(b"/link"):
            return frame(lt.RESP_COMMAND_MAGIC, b"link up")
        return frame(lt.RESP_DATA_MAGIC, b"")

    def read_frame(self) -> bytes:
        if not self._stable():
            return bytes(lt.FRAME_SIZE)
        return frame(lt.RESP_DATA_MAGIC, b"")


class RetryingCommandBus:
    def __init__(self) -> None:
        self.command_writes = 0

    def __enter__(self) -> "RetryingCommandBus":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None

    def write_frame(self, raw: bytes) -> bytes:
        magic = raw[0]
        payload_len = raw[1]
        payload = bytes(raw[2 : 2 + payload_len])
        if magic == lt.REQ_COMMAND_MAGIC and payload.startswith(b"/link"):
            self.command_writes += 1
            if self.command_writes == 1:
                return bytes(lt.FRAME_SIZE)
            return frame(lt.RESP_COMMAND_MAGIC, b"link up")
        return frame(lt.RESP_DATA_MAGIC, b"")

    def read_frame(self) -> bytes:
        return bytes(lt.FRAME_SIZE)


class LinkTerminalTimingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.prompt = FakePrompt()
        self.stream = FakeStreamPrinter()
        self.clock = FakeClock()

    def test_send_then_command_waits_for_transaction_gap(self) -> None:
        bus = TimingSensitiveBus(self.clock, min_gap_s=0.05)
        throttle = lt.TransactionThrottle(min_gap_s=0.05)

        with mock.patch.object(lt, "open_bus", return_value=bus), mock.patch.object(
            lt.time, "monotonic", side_effect=self.clock.monotonic
        ), mock.patch.object(lt.time, "sleep", side_effect=self.clock.sleep):
            lt.exchange_frame(
                0,
                0,
                100_000,
                self.prompt,
                self.stream,
                lt.REQ_COMMAND_MAGIC,
                b"10.8.0.6: hey",
                0.05,
                False,
                throttle,
            )
            lt.exchange_frame(
                0,
                0,
                100_000,
                self.prompt,
                self.stream,
                lt.REQ_COMMAND_MAGIC,
                b"/link\n",
                0.05,
                True,
                throttle,
            )

        self.assertIn("link up", self.prompt.lines)
        self.assertGreaterEqual(self.clock.now, 0.05)

    def test_command_retries_after_invalid_zero_reply(self) -> None:
        bus = RetryingCommandBus()

        with mock.patch.object(lt, "open_bus", return_value=bus):
            lt.exchange_frame(
                0,
                0,
                100_000,
                self.prompt,
                self.stream,
                lt.REQ_COMMAND_MAGIC,
                b"/link\n",
                0.01,
                True,
            )

        self.assertEqual(bus.command_writes, 2)
        self.assertIn("link up", self.prompt.lines)


if __name__ == "__main__":
    unittest.main()
