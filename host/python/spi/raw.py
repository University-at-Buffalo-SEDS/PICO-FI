#!/usr/bin/env python3
"""Minimal raw SPI helpers built on spidev."""

from __future__ import annotations

try:
    import spidev
except ImportError as exc:  # pragma: no cover - runtime dependency
    raise SystemExit(
        "error: spidev is required. Install it with `python3 -m pip install spidev`."
    ) from exc

FRAME_SIZE = 258
REQ_MAGIC = 0xA5


def _empty_request_frame() -> bytes:
    frame = bytearray(FRAME_SIZE)
    frame[0] = REQ_MAGIC
    return bytes(frame)


class RawSpiBus:
    def __init__(self, bus: int, device: int, speed_hz: int, mode: int = 3):
        self.spi = spidev.SpiDev()
        self.spi.open(bus, device)
        self.spi.mode = mode
        self.spi.bits_per_word = 8
        self.spi.max_speed_hz = speed_hz

    def close(self) -> None:
        self.spi.close()

    def transfer(self, tx: bytes) -> bytes:
        return bytes(self.spi.xfer2(list(tx)))

    def write_frame(self, frame: bytes) -> bytes:
        return self.transfer(frame)

    def read_frame(self) -> bytes:
        return self.transfer(_empty_request_frame())


def open_bus(bus: int, device: int, speed_hz: int) -> RawSpiBus:
    return RawSpiBus(bus, device, speed_hz)
