#!/usr/bin/env python3
"""Tests for the one-shot telemetry CLI helpers."""

from __future__ import annotations

import argparse
import base64
import io
import types
import unittest
from contextlib import redirect_stdout
from unittest import mock

import sys

if "spidev" not in sys.modules:
    sys.modules["spidev"] = types.SimpleNamespace(SpiDev=object)

from host.python import telemetry_cli


class FakePacket:
    def __init__(self, sender: str, payload: bytes, timestamp_ms: int = 0) -> None:
        self.sender = sender
        self.payload = payload
        self.timestamp_ms = timestamp_ms

    def serialize(self) -> bytes:
        return self.sender.encode("utf-8") + b"|" + str(self.timestamp_ms).encode("utf-8") + b"|" + self.payload


class FakeSedsprintf:
    class DataType:
        MESSAGE_DATA = 7

    class DataEndpoint:
        GROUND_STATION = 3

    @staticmethod
    def make_packet(packet_type: int, sender: str, endpoints: list[int], timestamp_ms: int,
                    payload: bytes) -> FakePacket:
        return FakePacket(sender, payload, timestamp_ms)

    @staticmethod
    def deserialize_packet_py(raw: bytes) -> FakePacket:
        sender_raw, timestamp_raw, payload = raw.split(b"|", 2)
        return FakePacket(sender_raw.decode("utf-8"), payload, int(timestamp_raw))


class FakeAdapter:
    payload_limit = 1024

    def __init__(self, incoming: bytes | None = None) -> None:
        self.incoming = incoming
        self.sent: list[bytes] = []
        self.closed = False

    def send_payload(self, payload: bytes) -> None:
        self.sent.append(payload)

    def recv_payload(self, timeout_s: float) -> bytes | None:
        payload = self.incoming
        self.incoming = None
        return payload

    def close(self) -> None:
        self.closed = True


class TelemetryCliTests(unittest.TestCase):
    def test_run_send_wraps_text_in_armored_packet(self) -> None:
        adapter = FakeAdapter()
        args = argparse.Namespace(
            sender="uart-node",
            packet_type="MESSAGE_DATA",
            endpoint=["GROUND_STATION"],
            text="hello telemetry",
            backend="uart",
        )

        with mock.patch.object(telemetry_cli, "load_sedsprintf", return_value=FakeSedsprintf), mock.patch.object(
                telemetry_cli, "build_adapter", return_value=adapter
        ), mock.patch.object(telemetry_cli.time, "time", return_value=12_345_678_901.234):
            rc = telemetry_cli.run_send(args)

        self.assertEqual(rc, 0)
        self.assertTrue(adapter.closed)
        self.assertEqual(len(adapter.sent), 1)
        armored = adapter.sent[0]
        self.assertTrue(armored.startswith(b"SP6:"))
        decoded = base64.urlsafe_b64decode(armored[4:])
        self.assertEqual(decoded, b"uart-node|12345678901234|hello telemetry")

    def test_build_packet_supports_large_timestamp_values(self) -> None:
        args = argparse.Namespace(
            sender="uart-node",
            packet_type="MESSAGE_DATA",
            endpoint=["GROUND_STATION"],
        )

        with mock.patch.object(telemetry_cli, "load_sedsprintf", return_value=FakeSedsprintf), mock.patch.object(
                telemetry_cli.time, "time", return_value=98_765_432_109.876):
            packet = telemetry_cli.build_packet(args, "big time")

        decoded = telemetry_cli.decode_packet(FakeSedsprintf, packet)
        self.assertIsNotNone(decoded)
        assert decoded is not None
        self.assertEqual(decoded.timestamp_ms, 98_765_432_109_876)
        self.assertEqual(decoded.payload, b"big time")

    def test_run_recv_prints_decoded_payload(self) -> None:
        packet = FakePacket("spi-node", b"rx payload")
        adapter = FakeAdapter(telemetry_cli.armor_packet(packet))
        args = argparse.Namespace(
            timeout=0.1,
            poll_interval=0.01,
            expect="rx payload",
            backend="spi",
        )
        out = io.StringIO()

        with mock.patch.object(telemetry_cli, "load_sedsprintf", return_value=FakeSedsprintf), mock.patch.object(
                telemetry_cli, "build_adapter", return_value=adapter
        ), redirect_stdout(out):
            rc = telemetry_cli.run_recv(args)

        self.assertEqual(rc, 0)
        self.assertTrue(adapter.closed)
        self.assertIn("sender=spi-node payload=rx payload", out.getvalue())

    def test_run_recv_accepts_raw_packet_payload(self) -> None:
        packet = FakePacket("spi-node", b"rx payload", 4_294_967_296)
        adapter = FakeAdapter(packet.serialize())
        args = argparse.Namespace(
            timeout=0.1,
            poll_interval=0.01,
            expect="rx payload",
            backend="spi",
        )
        out = io.StringIO()

        with mock.patch.object(telemetry_cli, "load_sedsprintf", return_value=FakeSedsprintf), mock.patch.object(
                telemetry_cli, "build_adapter", return_value=adapter
        ), redirect_stdout(out):
            rc = telemetry_cli.run_recv(args)

        self.assertEqual(rc, 0)
        self.assertTrue(adapter.closed)
        self.assertIn("sender=spi-node payload=rx payload", out.getvalue())


if __name__ == "__main__":
    unittest.main()
