#!/usr/bin/env python3
"""Unit tests for Ethernet bridge framing."""

from __future__ import annotations

import unittest

from host.python import bridge_framing


class BridgeFramingTests(unittest.TestCase):
    def test_build_bridge_frame_writes_magic_and_length(self) -> None:
        frame = bridge_framing.build_bridge_frame(b"hello")

        self.assertEqual(frame[:2], bytes([bridge_framing.BRIDGE_FRAME_MAGIC0, bridge_framing.BRIDGE_FRAME_MAGIC1]))
        self.assertEqual(frame[2:4], (5).to_bytes(2, "little"))
        self.assertEqual(frame[4:], b"hello")

    def test_decoder_handles_fragmented_and_coalesced_frames(self) -> None:
        decoder = bridge_framing.BridgeFrameDecoder()
        raw = (
            bridge_framing.build_bridge_frame(b"alpha")
            + bridge_framing.build_bridge_frame(b"")
            + bridge_framing.build_bridge_frame(bytes(range(16)))
        )

        outputs: list[bytes] = []
        for chunk in (raw[:1], raw[1:3], raw[3:9], raw[9:18], raw[18:]):
            outputs.extend(decoder.push(chunk))

        self.assertEqual(outputs, [b"alpha", b"", bytes(range(16))])

    def test_decoder_rejects_invalid_magic(self) -> None:
        decoder = bridge_framing.BridgeFrameDecoder()
        with self.assertRaisesRegex(ValueError, "invalid bridge frame magic"):
            decoder.push(b"\x00\x00\x01\x00x")


if __name__ == "__main__":
    unittest.main()
