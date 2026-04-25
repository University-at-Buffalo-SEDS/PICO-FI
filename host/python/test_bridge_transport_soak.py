#!/usr/bin/env python3
"""Long-running simulated end-to-end bridge soak tests."""

from __future__ import annotations

import itertools
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

from host.python import bridge_framing
from host.python import sedsprintf_router_common as common
from host.python.i2c import protocol as i2c_protocol
from host.python.spi import test as spi_test
from host.python.uart import test as uart_test

SOAK_BATCHES = 600
MAX_BATCH_SIZE = 4
COMMON_PAYLOAD_MAX = spi_test.PAYLOAD_MAX
ROUTER_PAYLOAD_MAX = 96


class FakePacket:
    def __init__(
            self,
            packet_type: int,
            sender: str,
            endpoints: list[int],
            timestamp_ms: int,
            payload: bytes,
    ) -> None:
        self.packet_type = packet_type
        self.sender = sender
        self.endpoints = endpoints
        self.timestamp_ms = timestamp_ms
        self.payload = payload

    def serialize(self) -> bytes:
        return b"|".join(
            [
                str(self.packet_type).encode("utf-8"),
                self.sender.encode("utf-8"),
                b",".join(str(endpoint).encode("utf-8") for endpoint in self.endpoints),
                str(self.timestamp_ms).encode("utf-8"),
                self.payload,
            ]
        )


class FakeSedsprintf:
    class DataType:
        MESSAGE_DATA = 7

    class DataEndpoint:
        GROUND_STATION = 3

    @staticmethod
    def deserialize_packet_py(raw: bytes) -> FakePacket:
        packet_type_raw, sender_raw, endpoints_raw, timestamp_raw, payload = raw.split(b"|", 4)
        endpoints = [int(value) for value in endpoints_raw.decode("utf-8").split(",") if value]
        return FakePacket(
            int(packet_type_raw),
            sender_raw.decode("utf-8"),
            endpoints,
            int(timestamp_raw),
            payload,
        )


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


class SimulatedUartLink:
    def __init__(self, rng: random.Random) -> None:
        self.rng = rng
        self.decoder = UartStreamDecoder()

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        raw = b"".join(uart_test.build_frame(payload, uart_test.REQ_DATA_MAGIC) for payload in payloads)
        outputs: list[bytes] = []
        cursor = 0
        while cursor < len(raw):
            step = min(len(raw) - cursor, self.rng.randint(1, 23))
            outputs.extend(self.decoder.push(raw[cursor:cursor + step]))
            cursor += step
        return outputs


class SimulatedSpiLink:
    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        outputs: list[bytes] = []
        for payload in payloads:
            frame = spi_test.build_frame(payload, spi_test.REQ_MAGIC)
            magic, _, decoded = spi_test.parse_frame(frame)
            if magic != spi_test.RESP_MAGIC:
                raise AssertionError("spi frame failed to parse during soak")
            outputs.append(decoded)
        return outputs


class SimulatedI2cLink:
    def __init__(self, rng: random.Random) -> None:
        self.rng = rng
        self.next_transfer_id = 1
        self.assembly: i2c_protocol.RxAssembly | None = None

    def _idle_slot(self) -> bytes:
        return bytes(i2c_protocol.SLOT_SIZE)

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        wire: list[bytes] = []
        for payload in payloads:
            transfer_id = self.next_transfer_id
            self.next_transfer_id = (self.next_transfer_id + 1) & 0xFFFF or 1
            wire.extend(
                i2c_protocol.encode_slots(
                    payload,
                    kind=i2c_protocol.KIND_DATA,
                    transfer_id=transfer_id,
                )
            )
            if self.rng.random() < 0.3:
                wire.append(self._idle_slot())

        outputs: list[bytes] = []
        for raw in wire:
            slot = i2c_protocol.decode_slot(raw)
            if slot is None:
                continue
            if slot.flags & i2c_protocol.FLAG_START:
                self.assembly = i2c_protocol.RxAssembly(slot)
                if slot.flags & i2c_protocol.FLAG_END:
                    outputs.append(bytes(slot.data))
                    self.assembly = None
                continue
            if self.assembly is None:
                raise AssertionError("i2c continuation slot arrived without active assembly")
            payload = self.assembly.push(slot)
            if payload is not None:
                outputs.append(payload)
                self.assembly = None
        return outputs


class SimulatedEthernetBridge:
    def __init__(self, rng: random.Random) -> None:
        self.rng = rng
        self.decoder = bridge_framing.BridgeFrameDecoder()

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        raw = b"".join(bridge_framing.build_bridge_frame(payload) for payload in payloads)
        outputs: list[bytes] = []
        cursor = 0
        while cursor < len(raw):
            step = min(len(raw) - cursor, self.rng.randint(1, 29))
            outputs.extend(self.decoder.push(raw[cursor:cursor + step]))
            cursor += step
        return outputs


def make_transport(name: str, rng: random.Random):
    if name == "uart":
        return SimulatedUartLink(rng)
    if name == "spi":
        return SimulatedSpiLink()
    if name == "i2c":
        return SimulatedI2cLink(rng)
    raise ValueError(f"unknown transport {name}")


def make_payload_batch(
        rng: random.Random,
        batch_size: int,
        prefix: bytes,
        timestamp_base_ms: int,
) -> list[tuple[bytes, int, bytes]]:
    fixed_lengths = [0, 1, 2, 3, 4, 5, 14, 15, 31, 32, 63, 64, 95, 96]
    outputs: list[tuple[bytes, int, bytes]] = []
    for index in range(batch_size):
        if rng.random() < 0.5:
            length = fixed_lengths[rng.randrange(len(fixed_lengths))]
        else:
            length = rng.randrange(ROUTER_PAYLOAD_MAX + 1)
        payload = bytearray(length)
        for offset in range(length):
            payload[offset] = rng.randrange(256)
        marker = prefix + f":{index:02d}:".encode("ascii")
        payload[: len(marker)] = marker[:len(payload)]
        payload_bytes = bytes(payload)
        timestamp_ms = timestamp_base_ms + index * 1_111_111_111
        packet = FakePacket(
            FakeSedsprintf.DataType.MESSAGE_DATA,
            prefix.decode("ascii", errors="replace"),
            [FakeSedsprintf.DataEndpoint.GROUND_STATION],
            timestamp_ms,
            payload_bytes,
        )
        armored = common.armor_packet(packet)
        if len(armored) > COMMON_PAYLOAD_MAX:
            raise AssertionError(
                f"generated armored router packet exceeded shared payload budget: {len(armored)}"
            )
        outputs.append((armored, timestamp_ms, payload_bytes))
    return outputs


class BridgeTransportSoakTests(unittest.TestCase):
    def test_message_boundaries_survive_long_transport_matrix_soak(self) -> None:
        for source_name, dest_name in itertools.product(("uart", "spi", "i2c"), repeat=2):
            seed = 0xA11CE000 + sum(ord(ch) for ch in f"{source_name}->{dest_name}")
            with self.subTest(source=source_name, dest=dest_name, seed=seed):
                rng = random.Random(seed)
                source_ingress = make_transport(source_name, rng)
                forward_ethernet = SimulatedEthernetBridge(rng)
                dest_egress = make_transport(dest_name, rng)

                dest_ingress = make_transport(dest_name, rng)
                reverse_ethernet = SimulatedEthernetBridge(rng)
                source_egress = make_transport(source_name, rng)

                for iteration in range(SOAK_BATCHES):
                    forward_batch_info = make_payload_batch(
                        rng,
                        rng.randint(1, MAX_BATCH_SIZE),
                        f"{source_name}->{dest_name}:{iteration}".encode("ascii"),
                        10_000_000_000_000 + iteration * 10_000_000_000,
                    )
                    reverse_batch_info = make_payload_batch(
                        rng,
                        rng.randint(1, MAX_BATCH_SIZE),
                        f"{dest_name}->{source_name}:{iteration}".encode("ascii"),
                        20_000_000_000_000 + iteration * 10_000_000_000,
                    )
                    forward_batch = [payload for payload, _, _ in forward_batch_info]
                    reverse_batch = [payload for payload, _, _ in reverse_batch_info]

                    forwarded = source_ingress.roundtrip(forward_batch)
                    self.assertEqual(forwarded, forward_batch)
                    forwarded = forward_ethernet.roundtrip(forwarded)
                    self.assertEqual(forwarded, forward_batch)
                    forwarded = dest_egress.roundtrip(forwarded)
                    self.assertEqual(forwarded, forward_batch)
                    for delivered, (_, timestamp_ms, expected_payload) in zip(forwarded, forward_batch_info, strict=True):
                        decoded = common.decode_packet(FakeSedsprintf, delivered)
                        self.assertIsNotNone(decoded)
                        assert decoded is not None
                        self.assertEqual(decoded.timestamp_ms, timestamp_ms)
                        self.assertEqual(decoded.payload, expected_payload)

                    reversed_payloads = dest_ingress.roundtrip(reverse_batch)
                    self.assertEqual(reversed_payloads, reverse_batch)
                    reversed_payloads = reverse_ethernet.roundtrip(reversed_payloads)
                    self.assertEqual(reversed_payloads, reverse_batch)
                    reversed_payloads = source_egress.roundtrip(reversed_payloads)
                    self.assertEqual(reversed_payloads, reverse_batch)
                    for delivered, (_, timestamp_ms, expected_payload) in zip(
                            reversed_payloads, reverse_batch_info, strict=True
                    ):
                        decoded = common.decode_packet(FakeSedsprintf, delivered)
                        self.assertIsNotNone(decoded)
                        assert decoded is not None
                        self.assertEqual(decoded.timestamp_ms, timestamp_ms)
                        self.assertEqual(decoded.payload, expected_payload)


if __name__ == "__main__":
    unittest.main()
