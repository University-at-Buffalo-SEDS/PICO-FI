#!/usr/bin/env python3
"""Long-running software fuzz/soak validation for Pico-Fi router paths."""

from __future__ import annotations

import argparse
import base64
import itertools
import random
import sys
import time
import types
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

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

TRANSPORTS = ("uart", "spi", "i2c")
COMMON_PAYLOAD_MAX = spi_test.PAYLOAD_MAX
ROUTER_PAYLOAD_MAX = 96
UART_PIPE_CAPACITY = 8192
ETH_PIPE_CAPACITY = 8192
SPI_QUEUE_CAPACITY = 32
I2C_SLOT_QUEUE_CAPACITY = 256
MAX_BATCH_SIZE = 5


@dataclass(slots=True)
class PacketExpectation:
    encoded: bytes
    valid: bool
    timestamp_ms: int | None
    payload: bytes | None


@dataclass(slots=True)
class FuzzStats:
    iterations: int = 0
    valid_packets: int = 0
    invalid_packets: int = 0
    decoded_valid_packets: int = 0
    rejected_invalid_packets: int = 0
    uart_noise_chunks: int = 0
    uart_invalid_headers: int = 0
    spi_invalid_frames: int = 0
    i2c_invalid_slots: int = 0
    ethernet_invalid_frames: int = 0
    buffer_writes: int = 0
    buffer_reads: int = 0


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


class BoundedBytePipe:
    def __init__(self, capacity: int, stats: FuzzStats) -> None:
        self.capacity = capacity
        self.buf = bytearray()
        self.stats = stats

    def __bool__(self) -> bool:
        return bool(self.buf)

    def write(self, chunk: bytes) -> None:
        if len(self.buf) + len(chunk) > self.capacity:
            raise AssertionError(
                f"pipe capacity exceeded: {len(self.buf)} + {len(chunk)} > {self.capacity}"
            )
        self.buf.extend(chunk)
        self.stats.buffer_writes += 1

    def read(self, count: int) -> bytes:
        count = min(count, len(self.buf))
        out = bytes(self.buf[:count])
        del self.buf[:count]
        self.stats.buffer_reads += 1
        return out


class UartStreamDecoder:
    def __init__(self, stats: FuzzStats) -> None:
        self.buf = bytearray()
        self.stats = stats

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
                self.stats.uart_invalid_headers += 1
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
            self.stats.uart_invalid_headers += 1
            self.buf.clear()
            return []

        frame_len = uart_test.FRAME_HEADER_SIZE + payload_len
        if len(self.buf) < frame_len:
            return []

        frame = bytes(self.buf[:frame_len])
        del self.buf[:frame_len]
        magic, _, payload = uart_test.parse_frame(frame)
        if magic != uart_test.RESP_DATA_MAGIC:
            self.stats.uart_invalid_headers += 1
            return []
        return [payload]


class SimulatedUartLink:
    def __init__(self, rng: random.Random, stats: FuzzStats) -> None:
        self.rng = rng
        self.stats = stats
        self.pipe = BoundedBytePipe(UART_PIPE_CAPACITY, stats)
        self.decoder = UartStreamDecoder(stats)

    def _noise_chunk(self) -> bytes:
        choice = self.rng.randrange(4)
        if choice == 0:
            length = self.rng.randint(1, 12)
            return bytes(self.rng.choice((0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x77)) for _ in range(length))
        if choice == 1:
            return bytes([uart_test.REQ_DATA_MAGIC, 0x00])
        if choice == 2:
            return bytes([uart_test.REQ_DATA_MAGIC, uart_test.RESP_DATA_MAGIC, 0xFF, 0xFF])
        return b"uart-noise"

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        segments: list[bytes] = []
        for payload in payloads:
            if self.rng.random() < 0.5:
                segments.append(self._noise_chunk())
                self.stats.uart_noise_chunks += 1
            segments.append(uart_test.build_frame(payload, uart_test.REQ_DATA_MAGIC))
            if self.rng.random() < 0.2:
                segments.append(self._noise_chunk())
                self.stats.uart_noise_chunks += 1

        outputs: list[bytes] = []
        segment_index = 0
        while segment_index < len(segments) or self.pipe:
            if segment_index < len(segments) and (not self.pipe or self.rng.random() < 0.7):
                segment = segments[segment_index]
                segment_index += 1
                offset = 0
                while offset < len(segment):
                    step = min(len(segment) - offset, self.rng.randint(1, 23))
                    self.pipe.write(segment[offset:offset + step])
                    offset += step
                    if self.pipe and self.rng.random() < 0.6:
                        outputs.extend(self.decoder.push(self.pipe.read(self.rng.randint(1, 31))))
            if self.pipe:
                outputs.extend(self.decoder.push(self.pipe.read(self.rng.randint(1, 31))))
        return outputs


class SimulatedSpiLink:
    def __init__(self, rng: random.Random, stats: FuzzStats) -> None:
        self.rng = rng
        self.stats = stats

    def _invalid_frame(self) -> bytes:
        choice = self.rng.randrange(3)
        if choice == 0:
            return b"\x00\x00\x00\x00"
        if choice == 1:
            frame = bytearray(spi_test.FRAME_HEADER_SIZE + 4)
            frame[0] = spi_test.REQ_MAGIC
            frame[1] = spi_test.RESP_MAGIC
            frame[2:4] = (spi_test.PAYLOAD_MAX + 1).to_bytes(2, "little")
            return bytes(frame)
        return bytes([spi_test.REQ_COMMAND_MAGIC, 0x00, 0x00, 0x00])

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        pending: list[tuple[bool, bytes]] = []
        for payload in payloads:
            if self.rng.random() < 0.35:
                pending.append((False, self._invalid_frame()))
            pending.append((True, spi_test.build_frame(payload, spi_test.REQ_MAGIC)))
        if len(pending) > SPI_QUEUE_CAPACITY:
            raise AssertionError(f"spi queue overflow: {len(pending)} > {SPI_QUEUE_CAPACITY}")

        outputs: list[bytes] = []
        for is_valid, frame in pending:
            magic, _, payload = spi_test.parse_frame(frame)
            if not is_valid:
                if magic != 0:
                    raise AssertionError("invalid SPI frame unexpectedly parsed as valid")
                self.stats.spi_invalid_frames += 1
                continue
            if magic != spi_test.RESP_MAGIC:
                raise AssertionError("valid SPI frame failed to parse")
            outputs.append(payload)
        return outputs


class SimulatedI2cLink:
    def __init__(self, rng: random.Random, stats: FuzzStats) -> None:
        self.rng = rng
        self.stats = stats
        self.next_transfer_id = 1
        self.assembly: i2c_protocol.RxAssembly | None = None

    def _idle_slot(self) -> bytes:
        return bytes(i2c_protocol.SLOT_SIZE)

    def _invalid_slots(self) -> list[bytes]:
        choice = self.rng.randrange(4)
        if choice == 0:
            bad = bytearray(i2c_protocol.SLOT_SIZE)
            bad[0] = 0x12
            bad[1] = 0x34
            return [bytes(bad)]
        if choice == 1:
            bad = bytearray(i2c_protocol.SLOT_SIZE)
            bad[0] = i2c_protocol.MAGIC0
            bad[1] = i2c_protocol.MAGIC1
            bad[2] = i2c_protocol.VERSION
            bad[3] = i2c_protocol.KIND_DATA
            bad[14:16] = (i2c_protocol.PAYLOAD_SIZE + 1).to_bytes(2, "little")
            return [bytes(bad)]
        if choice == 2:
            return [
                i2c_protocol.encode_slot(
                    i2c_protocol.KIND_DATA,
                    i2c_protocol.FLAG_START,
                    0x7777,
                    0,
                    5,
                    b"abc",
                ),
                i2c_protocol.encode_slot(
                    i2c_protocol.KIND_DATA,
                    i2c_protocol.FLAG_END,
                    0x7777,
                    99,
                    5,
                    b"de",
                ),
            ]
        return [self._idle_slot()]

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        wire: list[bytes] = []
        for payload in payloads:
            if self.rng.random() < 0.35:
                wire.extend(self._invalid_slots())
            transfer_id = self.next_transfer_id
            self.next_transfer_id = (self.next_transfer_id + 1) & 0xFFFF or 1
            wire.extend(
                i2c_protocol.encode_slots(
                    payload,
                    kind=i2c_protocol.KIND_DATA,
                    transfer_id=transfer_id,
                )
            )
            if self.rng.random() < 0.25:
                wire.append(self._idle_slot())

        if len(wire) > I2C_SLOT_QUEUE_CAPACITY:
            raise AssertionError(f"i2c slot queue overflow: {len(wire)} > {I2C_SLOT_QUEUE_CAPACITY}")

        outputs: list[bytes] = []
        for raw in wire:
            try:
                slot = i2c_protocol.decode_slot(raw)
            except ValueError:
                self.stats.i2c_invalid_slots += 1
                self.assembly = None
                continue
            if slot is None:
                continue
            if slot.flags & i2c_protocol.FLAG_START:
                self.assembly = i2c_protocol.RxAssembly(slot)
                if slot.flags & i2c_protocol.FLAG_END:
                    outputs.append(bytes(slot.data))
                    self.assembly = None
                continue
            if self.assembly is None:
                self.stats.i2c_invalid_slots += 1
                continue
            try:
                payload = self.assembly.push(slot)
            except ValueError:
                self.stats.i2c_invalid_slots += 1
                self.assembly = None
                continue
            if payload is not None:
                outputs.append(payload)
                self.assembly = None
        return outputs


class SimulatedEthernetBridge:
    def __init__(self, rng: random.Random, stats: FuzzStats) -> None:
        self.rng = rng
        self.stats = stats
        self.decoder = bridge_framing.BridgeFrameDecoder()
        self.pipe = BoundedBytePipe(ETH_PIPE_CAPACITY, stats)

    def roundtrip(self, payloads: list[bytes]) -> list[bytes]:
        outputs: list[bytes] = []
        for payload in payloads:
            frame = bridge_framing.build_bridge_frame(payload)
            offset = 0
            while offset < len(frame):
                step = min(len(frame) - offset, self.rng.randint(1, 29))
                self.pipe.write(frame[offset:offset + step])
                offset += step
                if self.pipe and self.rng.random() < 0.65:
                    outputs.extend(self.decoder.push(self.pipe.read(self.rng.randint(1, 37))))
            while self.pipe:
                outputs.extend(self.decoder.push(self.pipe.read(self.rng.randint(1, 37))))
        while self.pipe:
            outputs.extend(self.decoder.push(self.pipe.read(self.rng.randint(1, 37))))
        return outputs


def make_transport(name: str, rng: random.Random, stats: FuzzStats):
    if name == "uart":
        return SimulatedUartLink(rng, stats)
    if name == "spi":
        return SimulatedSpiLink(rng, stats)
    if name == "i2c":
        return SimulatedI2cLink(rng, stats)
    raise ValueError(f"unknown transport {name}")


def make_good_expectation(rng: random.Random, label: str, timestamp_ms: int) -> PacketExpectation:
    length = rng.randrange(ROUTER_PAYLOAD_MAX + 1)
    payload = bytearray(length)
    for offset in range(length):
        payload[offset] = rng.randrange(256)
    marker = label.encode("ascii")
    payload[: len(marker)] = marker[:len(payload)]
    packet = FakePacket(
        FakeSedsprintf.DataType.MESSAGE_DATA,
        label,
        [FakeSedsprintf.DataEndpoint.GROUND_STATION],
        timestamp_ms,
        bytes(payload),
    )
    encoded = common.armor_packet(packet)
    if len(encoded) > COMMON_PAYLOAD_MAX:
        raise AssertionError(f"valid router packet exceeded payload budget: {len(encoded)}")
    return PacketExpectation(encoded, True, timestamp_ms, bytes(payload))


def make_bad_expectation(rng: random.Random, label: str) -> PacketExpectation:
    choice = rng.randrange(5)
    if choice == 0:
        encoded = b"SP6:not-valid-base64!"
    elif choice == 1:
        encoded = common.ARMOR_PREFIX + b"Zm9v"
    elif choice == 2:
        encoded = b"7|" + label.encode("ascii") + b"|3|not-a-timestamp|payload"
    elif choice == 3:
        encoded = (
            common.ARMOR_PREFIX
            + base64.urlsafe_b64encode(b"7|" + label.encode("ascii") + b"|3|broken")
        )
    else:
        encoded = b"broken-packet-" + label.encode("ascii")
    if len(encoded) > COMMON_PAYLOAD_MAX:
        encoded = encoded[:COMMON_PAYLOAD_MAX]
    return PacketExpectation(encoded, False, None, None)


def make_batch(
    rng: random.Random,
    source_name: str,
    dest_name: str,
    iteration: int,
    stats: FuzzStats,
) -> list[PacketExpectation]:
    batch_size = rng.randint(1, MAX_BATCH_SIZE)
    expectations: list[PacketExpectation] = []
    timestamp_base_ms = 10_000_000_000_000 + iteration * 10_000_000_000
    for index in range(batch_size):
        label = f"{source_name}->{dest_name}:{iteration}:{index}"
        if rng.random() < 0.65:
            expectations.append(
                make_good_expectation(rng, label, timestamp_base_ms + index * 1_111_111_111)
            )
            stats.valid_packets += 1
        else:
            expectations.append(make_bad_expectation(rng, label))
            stats.invalid_packets += 1
    return expectations


def verify_payloads(expectations: list[PacketExpectation], delivered: list[bytes], context: str, stats: FuzzStats) -> None:
    if delivered != [item.encoded for item in expectations]:
        raise AssertionError(f"{context}: transport payload mismatch")
    for expected, payload in zip(expectations, delivered, strict=True):
        decoded = common.decode_packet(FakeSedsprintf, payload)
        if expected.valid:
            if decoded is None:
                raise AssertionError(f"{context}: valid router packet failed to decode")
            if decoded.timestamp_ms != expected.timestamp_ms:
                raise AssertionError(
                    f"{context}: timestamp mismatch {decoded.timestamp_ms} != {expected.timestamp_ms}"
                )
            if bytes(decoded.payload) != expected.payload:
                raise AssertionError(f"{context}: payload mismatch after decode")
            stats.decoded_valid_packets += 1
        else:
            if decoded is not None:
                raise AssertionError(f"{context}: invalid router packet decoded unexpectedly")
            stats.rejected_invalid_packets += 1


def run_fuzz(duration_s: float, seed: int, status_interval_s: float) -> None:
    rng = random.Random(seed)
    stats = FuzzStats()
    forward_ethernet = SimulatedEthernetBridge(rng, stats)
    reverse_ethernet = SimulatedEthernetBridge(rng, stats)
    ingress = {name: make_transport(name, rng, stats) for name in TRANSPORTS}
    egress = {name: make_transport(name, rng, stats) for name in TRANSPORTS}
    pairs = list(itertools.product(TRANSPORTS, repeat=2))

    start = time.monotonic()
    deadline = start + duration_s
    next_status = start + status_interval_s
    pair_index = 0

    while time.monotonic() < deadline:
        source_name, dest_name = pairs[pair_index % len(pairs)]
        pair_index += 1
        expectations = make_batch(rng, source_name, dest_name, stats.iterations, stats)
        reverse_expectations = make_batch(rng, dest_name, source_name, stats.iterations, stats)

        forward = ingress[source_name].roundtrip([item.encoded for item in expectations])
        forward = forward_ethernet.roundtrip(forward)
        forward = egress[dest_name].roundtrip(forward)
        verify_payloads(expectations, forward, f"{source_name}->{dest_name}", stats)

        reverse = ingress[dest_name].roundtrip([item.encoded for item in reverse_expectations])
        reverse = reverse_ethernet.roundtrip(reverse)
        reverse = egress[source_name].roundtrip(reverse)
        verify_payloads(reverse_expectations, reverse, f"{dest_name}->{source_name}", stats)

        stats.iterations += 1
        now = time.monotonic()
        if now >= next_status:
            print_status(stats, seed, duration_s, start, now)
            next_status = now + status_interval_s

    print_status(stats, seed, duration_s, start, time.monotonic(), final=True)


def print_status(
    stats: FuzzStats,
    seed: int,
    duration_s: float,
    start: float,
    now: float,
    final: bool = False,
) -> None:
    elapsed = now - start
    progress = min(max(elapsed / duration_s, 0.0), 1.0) if duration_s > 0 else 1.0
    iter_rate = stats.iterations / elapsed if elapsed > 0 else 0.0
    valid_rate = stats.decoded_valid_packets / elapsed if elapsed > 0 else 0.0
    prefix = "[fuzz-final]" if final else "[fuzz]"
    print(
        f"{prefix} seed={seed} progress={progress * 100:5.1f}% elapsed={elapsed:.1f}s/{duration_s:.1f}s "
        f"iterations={stats.iterations} "
        f"iter_rate={iter_rate:,.0f}/s valid_rate={valid_rate:,.0f}/s "
        f"valid={stats.valid_packets}/{stats.decoded_valid_packets} "
        f"invalid={stats.invalid_packets}/{stats.rejected_invalid_packets} "
        f"uart_noise={stats.uart_noise_chunks} uart_reject={stats.uart_invalid_headers} "
        f"spi_reject={stats.spi_invalid_frames} i2c_reject={stats.i2c_invalid_slots} "
        f"eth_reject={stats.ethernet_invalid_frames} "
        f"pipe_rw={stats.buffer_writes}/{stats.buffer_reads}",
        flush=True,
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration-s", type=float, default=600.0)
    parser.add_argument("--status-s", type=float, default=30.0)
    parser.add_argument("--seed", type=int, default=0xC0FFEE)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    run_fuzz(args.duration_s, args.seed, args.status_s)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
