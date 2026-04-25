#!/usr/bin/env python3
"""Unit tests for UART/SPI/I2C framing roundtrips."""

from __future__ import annotations

import random
import sys
import types
import unittest

sys.modules.setdefault(
    "serial",
    types.SimpleNamespace(
        Serial=object,
        SerialException=Exception,
        EIGHTBITS=8,
        PARITY_NONE="N",
        STOPBITS_ONE=1,
    ),
)
sys.modules.setdefault("spidev", types.SimpleNamespace(SpiDev=object))

from host.python.i2c import protocol as i2c_protocol
from host.python.spi import test as spi_test
from host.python.uart import test as uart_test


class UartStreamDecoder:
    def __init__(self) -> None:
        self.buf = bytearray()

    def push(self, chunk: bytes) -> list[bytes]:
        outputs: list[bytes] = []
        for byte in chunk:
            outputs.extend(self.push_byte(byte))
        return outputs

    def push_byte(self, byte: int) -> list[bytes]:
        if not self.buf:
            if byte not in (uart_test.REQ_DATA_MAGIC, uart_test.REQ_COMMAND_MAGIC):
                return []
            self.buf.append(byte)
            return []

        if len(self.buf) == 1:
            expected = (
                uart_test.RESP_COMMAND_MAGIC
                if self.buf[0] == uart_test.REQ_COMMAND_MAGIC
                else uart_test.RESP_DATA_MAGIC
            )
            if byte != expected:
                if byte in (uart_test.REQ_DATA_MAGIC, uart_test.REQ_COMMAND_MAGIC):
                    self.buf[:] = bytes([byte])
                else:
                    self.buf.clear()
                return []
            self.buf.append(byte)
            return []

        self.buf.append(byte)
        if len(self.buf) < uart_test.FRAME_HEADER_SIZE:
            return []

        payload_len = int.from_bytes(self.buf[2:4], "little")
        if payload_len > uart_test.PAYLOAD_MAX:
            self.buf.clear()
            return []

        frame_len = uart_test.FRAME_HEADER_SIZE + payload_len
        if len(self.buf) < frame_len:
            return []

        frame = bytes(self.buf[:frame_len])
        del self.buf[:frame_len]
        magic, _, payload = uart_test.parse_frame(frame)
        if magic != uart_test.RESP_DATA_MAGIC:
            return []
        return [payload]


class TransportRoundtripTests(unittest.TestCase):
    def test_uart_stream_roundtrip_survives_fragmentation(self) -> None:
        rng = random.Random(0xC0DE)
        payloads = [bytes(rng.randrange(256) for _ in range(length)) for length in range(64)]
        raw = b"".join(uart_test.build_frame(payload, uart_test.REQ_DATA_MAGIC) for payload in payloads)
        decoder = UartStreamDecoder()
        outputs: list[bytes] = []
        cursor = 0
        while cursor < len(raw):
            step = min(len(raw) - cursor, rng.randint(1, 17))
            outputs.extend(decoder.push(raw[cursor:cursor + step]))
            cursor += step

        self.assertEqual(outputs, payloads)

    def test_spi_roundtrip_preserves_payloads(self) -> None:
        cases = [
            b"",
            b"a",
            bytes(range(32)),
            bytes(range(spi_test.PAYLOAD_MAX)),
        ]
        for payload in cases:
            with self.subTest(length=len(payload)):
                frame = spi_test.build_frame(payload, spi_test.REQ_MAGIC)
                magic, length, decoded = spi_test.parse_frame(frame)
                self.assertEqual(magic, spi_test.RESP_MAGIC)
                self.assertEqual(length, len(payload))
                self.assertEqual(decoded, payload)

    def test_i2c_slot_roundtrip_reassembles_large_payload(self) -> None:
        payload = bytes((index * 17) & 0xFF for index in range(300))
        slots = i2c_protocol.encode_slots(
            payload,
            kind=i2c_protocol.KIND_DATA,
            transfer_id=0x1234,
        )

        assembly: i2c_protocol.RxAssembly | None = None
        result: bytes | None = None
        for raw in slots:
            slot = i2c_protocol.decode_slot(raw)
            self.assertIsNotNone(slot)
            assert slot is not None
            if slot.flags & i2c_protocol.FLAG_START:
                assembly = i2c_protocol.RxAssembly(slot)
                if slot.flags & i2c_protocol.FLAG_END:
                    result = bytes(slot.data)
            else:
                self.assertIsNotNone(assembly)
                assert assembly is not None
                result = assembly.push(slot)

        self.assertEqual(result, payload)


if __name__ == "__main__":
    unittest.main()
