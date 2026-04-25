#!/usr/bin/env python3
"""Host-side helpers for the Pico-Fi Ethernet bridge frame."""

from __future__ import annotations

BRIDGE_FRAME_HEADER_SIZE = 4
BRIDGE_FRAME_MAGIC0 = 0xB5
BRIDGE_FRAME_MAGIC1 = 0x4E
BRIDGE_FRAME_MAX_PAYLOAD = 0xFFFF


def build_bridge_frame(payload: bytes) -> bytes:
    """Encode one Ethernet bridge frame."""
    if len(payload) > BRIDGE_FRAME_MAX_PAYLOAD:
        raise ValueError(f"bridge payload too large: {len(payload)}")
    header = bytearray(BRIDGE_FRAME_HEADER_SIZE)
    header[0] = BRIDGE_FRAME_MAGIC0
    header[1] = BRIDGE_FRAME_MAGIC1
    header[2:4] = len(payload).to_bytes(2, "little")
    return bytes(header) + payload


class BridgeFrameDecoder:
    """Incrementally decode Ethernet bridge frames from a byte stream."""

    def __init__(self) -> None:
        self.buf = bytearray()

    def push(self, chunk: bytes) -> list[bytes]:
        self.buf.extend(chunk)
        out: list[bytes] = []
        while True:
            if len(self.buf) < BRIDGE_FRAME_HEADER_SIZE:
                return out
            if self.buf[0] != BRIDGE_FRAME_MAGIC0 or self.buf[1] != BRIDGE_FRAME_MAGIC1:
                raise ValueError(
                    f"invalid bridge frame magic: 0x{self.buf[0]:02x} 0x{self.buf[1]:02x}"
                )
            payload_len = int.from_bytes(self.buf[2:4], "little")
            frame_len = BRIDGE_FRAME_HEADER_SIZE + payload_len
            if len(self.buf) < frame_len:
                return out
            out.append(bytes(self.buf[BRIDGE_FRAME_HEADER_SIZE:frame_len]))
            del self.buf[:frame_len]

