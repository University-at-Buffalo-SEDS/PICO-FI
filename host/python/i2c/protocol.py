#!/usr/bin/env python3
"""Chunked packet framing for the Pico-Fi I2C transport."""

from __future__ import annotations

import time
from dataclasses import dataclass

SLOT_SIZE = 32
HEADER_SIZE = 18
PAYLOAD_SIZE = SLOT_SIZE - HEADER_SIZE

MAGIC0 = 0x49
MAGIC1 = 0x32
VERSION = 1

KIND_IDLE = 0x00
KIND_DATA = 0x01
KIND_COMMAND = 0x02
KIND_ERROR = 0x7F

FLAG_START = 0x01
FLAG_END = 0x02


@dataclass(slots=True)
class Slot:
    kind: int
    flags: int
    transfer_id: int
    offset: int
    total_len: int
    data: bytes


class RxAssembly:
    def __init__(self, slot: Slot) -> None:
        if not (slot.flags & FLAG_START):
            raise ValueError("transfer must start with START flag")
        if slot.offset != 0:
            raise ValueError("transfer start must have offset 0")
        if len(slot.data) > slot.total_len:
            raise ValueError("first slot exceeds total length")
        self.kind = slot.kind
        self.transfer_id = slot.transfer_id
        self.total_len = slot.total_len
        self.next_offset = len(slot.data)
        self.payload = bytearray(slot.data)

    def push(self, slot: Slot) -> bytes | None:
        if slot.kind != self.kind:
            raise ValueError("transfer kind changed mid-stream")
        if slot.transfer_id != self.transfer_id:
            raise ValueError("transfer id changed mid-stream")
        if slot.offset != self.next_offset:
            raise ValueError(
                f"transfer offset mismatch: expected {self.next_offset} got {slot.offset}"
            )
        if len(self.payload) + len(slot.data) > self.total_len:
            raise ValueError("transfer exceeded declared total length")
        if slot.offset != 0:
            self.payload.extend(slot.data)
            self.next_offset += len(slot.data)
        if slot.flags & FLAG_END:
            if len(self.payload) != self.total_len:
                raise ValueError(
                    f"transfer ended early: expected {self.total_len} got {len(self.payload)}"
                )
            return bytes(self.payload)
        return None


def encode_slot(kind: int, flags: int, transfer_id: int, offset: int, total_len: int, data: bytes) -> bytes:
    data = data[:PAYLOAD_SIZE]
    raw = bytearray(SLOT_SIZE)
    raw[0] = MAGIC0
    raw[1] = MAGIC1
    raw[2] = VERSION
    raw[3] = kind & 0xFF
    raw[4] = flags & 0xFF
    raw[5] = 0
    raw[6:10] = int(offset).to_bytes(4, "little", signed=False)
    raw[10:14] = int(total_len).to_bytes(4, "little", signed=False)
    raw[14:16] = len(data).to_bytes(2, "little", signed=False)
    raw[16:18] = int(transfer_id & 0xFFFF).to_bytes(2, "little", signed=False)
    raw[HEADER_SIZE:HEADER_SIZE + len(data)] = data
    return bytes(raw)


def decode_slot(raw: bytes) -> Slot | None:
    if len(raw) != SLOT_SIZE:
        raise ValueError(f"expected {SLOT_SIZE} bytes, got {len(raw)}")
    if all(byte == 0x00 for byte in raw) or all(byte == 0xFF for byte in raw):
        return None
    if raw[0] != MAGIC0 or raw[1] != MAGIC1:
        raise ValueError(f"invalid slot magic: {raw[0]:02x} {raw[1]:02x}")
    if raw[2] != VERSION:
        raise ValueError(f"unsupported slot version: {raw[2]}")
    kind = raw[3]
    if kind == KIND_IDLE:
        return None
    data_len = int.from_bytes(raw[14:16], "little", signed=False)
    if data_len > PAYLOAD_SIZE:
        raise ValueError(f"invalid slot payload length: {data_len}")
    return Slot(
        kind=kind,
        flags=raw[4],
        transfer_id=int.from_bytes(raw[16:18], "little", signed=False),
        offset=int.from_bytes(raw[6:10], "little", signed=False),
        total_len=int.from_bytes(raw[10:14], "little", signed=False),
        data=bytes(raw[HEADER_SIZE:HEADER_SIZE + data_len]),
    )


def encode_slots(payload: bytes, *, kind: int, transfer_id: int) -> list[bytes]:
    if not payload:
        return [encode_slot(kind, FLAG_START | FLAG_END, transfer_id, 0, 0, b"")]
    out: list[bytes] = []
    total_len = len(payload)
    for offset in range(0, total_len, PAYLOAD_SIZE):
        chunk = payload[offset:offset + PAYLOAD_SIZE]
        flags = 0
        if offset == 0:
            flags |= FLAG_START
        if offset + len(chunk) >= total_len:
            flags |= FLAG_END
        out.append(encode_slot(kind, flags, transfer_id, offset, total_len, chunk))
    return out


def write_packet(bus, addr: int, payload: bytes, *, kind: int, transfer_id: int, chunk_delay_s: float) -> None:
    slots = encode_slots(payload, kind=kind, transfer_id=transfer_id)
    for idx, slot in enumerate(slots):
        bus.write(addr, slot)
        if idx + 1 < len(slots):
            time.sleep(chunk_delay_s)


def read_slot(bus, addr: int) -> Slot | None:
    return decode_slot(bus.read(addr, SLOT_SIZE))


def read_packet(bus, addr: int, *, chunk_delay_s: float, timeout_s: float) -> tuple[int, bytes] | None:
    deadline = time.monotonic() + timeout_s
    assembly: RxAssembly | None = None
    while time.monotonic() < deadline:
        slot = read_slot(bus, addr)
        if slot is None:
            time.sleep(chunk_delay_s)
            continue
        if slot.flags & FLAG_START:
            assembly = RxAssembly(slot)
            if slot.flags & FLAG_END:
                return slot.kind, bytes(slot.data)
            time.sleep(chunk_delay_s)
            continue
        if assembly is None:
            raise ValueError("slot arrived without an active transfer")
        assert assembly is not None
        payload = assembly.push(slot)
        if payload is not None:
            return slot.kind, payload
        time.sleep(chunk_delay_s)
    return None
