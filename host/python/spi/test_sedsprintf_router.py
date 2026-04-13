#!/usr/bin/env python3
"""Regression tests for the SPI telemetry router adapter."""

from __future__ import annotations

import types
import unittest
from unittest import mock

import sys

if "spidev" not in sys.modules:
    sys.modules["spidev"] = types.SimpleNamespace(SpiDev=object)

from host.python.spi import sedsprintf_router as spi_router


def frame(magic: int, payload: bytes = b"") -> bytes:
    return spi_router.build_frame(payload, magic)


class FakeBus:
    def __init__(self, write_frames: list[bytes], read_frames: list[bytes]) -> None:
        self.write_frames = list(write_frames)
        self.read_frames = list(read_frames)
        self.writes: list[bytes] = []
        self.closed = False

    def write_frame(self, raw: bytes) -> bytes:
        self.writes.append(raw)
        if self.write_frames:
            return self.write_frames.pop(0)
        return bytes(spi_router.FRAME_SIZE)

    def read_frame(self) -> bytes:
        if self.read_frames:
            return self.read_frames.pop(0)
        return bytes(spi_router.FRAME_SIZE)

    def close(self) -> None:
        self.closed = True


class SpiRouterAdapterTests(unittest.TestCase):
    def test_send_payload_uses_data_magic(self) -> None:
        bus = FakeBus([frame(spi_router.RESP_MAGIC, b"")], [])
        with mock.patch.object(spi_router, "open_bus", return_value=bus):
            adapter = spi_router.SpiRouterAdapter(0, 0, 100_000)
            try:
                adapter.send_payload(b"telemetry")
            finally:
                adapter.close()

        self.assertEqual(bus.writes[0][0], spi_router.REQ_MAGIC)
        self.assertTrue(bus.closed)

    def test_recv_payload_uses_pull_command_and_returns_polled_data(self) -> None:
        bus = FakeBus(
            [frame(spi_router.RESP_MAGIC, b"")],
            [frame(spi_router.RESP_MAGIC, b"telemetry-rx")],
        )
        with mock.patch.object(spi_router, "open_bus", return_value=bus):
            adapter = spi_router.SpiRouterAdapter(0, 0, 100_000)
            try:
                payload = adapter.recv_payload(0.1)
            finally:
                adapter.close()

        self.assertEqual(payload, b"telemetry-rx")
        self.assertEqual(bus.writes[0], frame(spi_router.REQ_COMMAND_MAGIC, spi_router.PULL_COMMAND))

    def test_recv_payload_returns_data_from_initial_pull_write(self) -> None:
        bus = FakeBus([frame(spi_router.RESP_MAGIC, b"telemetry-rx")], [])
        with mock.patch.object(spi_router, "open_bus", return_value=bus):
            adapter = spi_router.SpiRouterAdapter(0, 0, 100_000)
            try:
                payload = adapter.recv_payload(0.1)
            finally:
                adapter.close()

        self.assertEqual(payload, b"telemetry-rx")
        self.assertEqual(len(bus.writes), 1)


if __name__ == "__main__":
    unittest.main()
