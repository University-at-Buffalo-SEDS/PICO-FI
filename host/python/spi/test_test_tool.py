#!/usr/bin/env python3
"""Regression tests for the SPI test tool exchange behavior."""

from __future__ import annotations

import io
import types
import unittest
from contextlib import redirect_stdout
from unittest import mock

import sys

if "spidev" not in sys.modules:
    sys.modules["spidev"] = types.SimpleNamespace(SpiDev=object)

from host.python.spi import test as spi_test


def frame(magic: int, payload: bytes = b"") -> bytes:
    return spi_test.build_frame(payload, magic)


class AckOnlyBus:
    def __init__(self) -> None:
        self.write_count = 0
        self.read_count = 0

    def close(self) -> None:
        return None

    def write_frame(self, raw: bytes) -> bytes:
        self.write_count += 1
        return frame(spi_test.RESP_MAGIC, b"")

    def read_frame(self) -> bytes:
        self.read_count += 1
        return bytes(spi_test.FRAME_SIZE)


class PullBus:
    def __init__(self) -> None:
        self.write_count = 0
        self.read_count = 0

    def close(self) -> None:
        return None

    def write_frame(self, raw: bytes) -> bytes:
        self.write_count += 1
        return frame(spi_test.RESP_MAGIC, b"")

    def read_frame(self) -> bytes:
        self.read_count += 1
        return frame(spi_test.RESP_MAGIC, b"uart-to-spi-1")


class TestToolExchangeTests(unittest.TestCase):
    def test_send_accepts_empty_data_ack_without_command_retries(self) -> None:
        bus = AckOnlyBus()
        with mock.patch.object(spi_test, "open_bus", return_value=bus):
            with redirect_stdout(io.StringIO()):
                rc = spi_test.spi_exchange(
                    0,
                    0,
                    100_000,
                    b"spi-to-uart-1",
                    spi_test.REQ_COMMAND_MAGIC,
                )

        self.assertEqual(rc, 0)
        self.assertEqual(bus.write_count, 1)
        self.assertEqual(bus.read_count, 0)

    def test_slash_command_still_waits_for_command_reply(self) -> None:
        bus = AckOnlyBus()
        with mock.patch.object(spi_test, "open_bus", return_value=bus):
            with redirect_stdout(io.StringIO()):
                rc = spi_test.spi_exchange(
                    0,
                    0,
                    100_000,
                    b"/link\n",
                    spi_test.REQ_COMMAND_MAGIC,
                    expect_command_reply=True,
                )

        self.assertEqual(rc, 1)
        self.assertGreaterEqual(bus.write_count, 1)
        self.assertGreater(bus.read_count, 0)

    def test_recv_via_pull_sends_followup_empty_poll_ack(self) -> None:
        bus = PullBus()
        with mock.patch.object(spi_test, "open_bus", return_value=bus):
            with redirect_stdout(io.StringIO()):
                rc = spi_test.spi_recv_via_pull(0, 0, 100_000, expected_text="uart-to-spi-1")

        self.assertEqual(rc, 0)
        self.assertEqual(bus.write_count, 1)
        self.assertEqual(bus.read_count, 1)


if __name__ == "__main__":
    unittest.main()
