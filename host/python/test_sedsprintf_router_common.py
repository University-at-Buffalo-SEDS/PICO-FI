#!/usr/bin/env python3
"""Regression tests for the shared sedsprintf router flow."""

from __future__ import annotations

import argparse
import base64
import unittest
from unittest import mock

from host.python import sedsprintf_router_common as common


class FakePacket:
    def __init__(self, packet_type: int, sender: str, endpoints: list[int], timestamp_ms: int, payload: bytes) -> None:
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
    def make_packet(packet_type: int, sender: str, endpoints: list[int], timestamp_ms: int,
                    payload: bytes) -> FakePacket:
        return FakePacket(packet_type, sender, endpoints, timestamp_ms, payload)

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


class FakeListenSocket:
    def __init__(self) -> None:
        self.bound_to: tuple[str, int] | None = None
        self.recv_calls = 0

    def bind(self, target: tuple[str, int]) -> None:
        self.bound_to = target

    def setblocking(self, flag: bool) -> None:
        return None

    def recvfrom(self, size: int) -> tuple[bytes, tuple[str, int]]:
        self.recv_calls += 1
        if self.recv_calls == 1:
            return b"udp-outbound", ("127.0.0.1", 54321)
        raise BlockingIOError

    def close(self) -> None:
        return None


class FakeForwardSocket:
    def __init__(self) -> None:
        self.sent: list[tuple[bytes, tuple[str, int]]] = []

    def sendto(self, payload: bytes, target: tuple[str, int]) -> None:
        self.sent.append((payload, target))

    def close(self) -> None:
        return None


class FakeAdapter:
    payload_limit = 1024

    def __init__(self) -> None:
        self.sent_payloads: list[bytes] = []
        self.recv_calls = 0

    def send_payload(self, payload: bytes) -> None:
        self.sent_payloads.append(payload)

    def recv_payload(self, timeout_s: float) -> bytes | None:
        self.recv_calls += 1
        if self.recv_calls == 1:
            packet = FakeSedsprintf.make_packet(
                FakeSedsprintf.DataType.MESSAGE_DATA,
                "spi-end",
                [FakeSedsprintf.DataEndpoint.GROUND_STATION],
                0,
                b"udp-inbound",
            )
            return common.armor_packet(packet)
        raise KeyboardInterrupt

    def close(self) -> None:
        return None


class RouterCommonTests(unittest.TestCase):
    def test_decode_packet_rejects_malformed_armored_and_raw_payloads(self) -> None:
        self.assertIsNone(common.decode_packet(FakeSedsprintf, b"SP6:not-valid-base64"))
        self.assertIsNone(common.decode_packet(FakeSedsprintf, b"not-a-packet"))

    def test_armor_and_decode_preserve_large_timestamps(self) -> None:
        for timestamp_ms in (0, 2**31, 2**32, 10**12, 10**15):
            with self.subTest(timestamp_ms=timestamp_ms):
                packet = FakePacket(
                    FakeSedsprintf.DataType.MESSAGE_DATA,
                    "router-end",
                    [FakeSedsprintf.DataEndpoint.GROUND_STATION],
                    timestamp_ms,
                    b"payload",
                )
                decoded = common.decode_packet(FakeSedsprintf, common.armor_packet(packet))
                self.assertIsNotNone(decoded)
                assert decoded is not None
                self.assertEqual(decoded.timestamp_ms, timestamp_ms)
                self.assertEqual(decoded.payload, b"payload")

    def test_run_udp_router_wraps_and_unwraps_telemetry_payloads(self) -> None:
        listen = FakeListenSocket()
        forward = FakeForwardSocket()
        adapter = FakeAdapter()
        args = argparse.Namespace(
            listen_host="127.0.0.1",
            listen_port=9000,
            forward_host="127.0.0.1",
            forward_port=9001,
            poll_ms=1,
            sender="uart-end",
            packet_type="MESSAGE_DATA",
            endpoint=["GROUND_STATION"],
            debug=False,
        )

        with mock.patch.object(common, "load_sedsprintf", return_value=FakeSedsprintf), mock.patch.object(
                common.socket,
                "socket",
                side_effect=[listen, forward],
        ), mock.patch.object(common.time, "time", return_value=9_876_543_210.123):
            rc = common.run_udp_router(adapter, args, "spi")

        self.assertEqual(rc, 0)
        self.assertEqual(listen.bound_to, ("127.0.0.1", 9000))
        self.assertEqual(forward.sent, [(b"udp-inbound", ("127.0.0.1", 9001))])
        self.assertEqual(len(adapter.sent_payloads), 1)
        armored = adapter.sent_payloads[0]
        self.assertTrue(armored.startswith(common.ARMOR_PREFIX))
        raw = base64.urlsafe_b64decode(armored[len(common.ARMOR_PREFIX):])
        packet = FakeSedsprintf.deserialize_packet_py(raw)
        self.assertEqual(packet.sender, "uart-end")
        self.assertEqual(packet.timestamp_ms, 9_876_543_210_123)
        self.assertEqual(packet.payload, b"udp-outbound")


if __name__ == "__main__":
    unittest.main()
