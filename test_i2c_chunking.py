#!/usr/bin/env python3
"""Validate the current 32-byte slot chunking used by Pico-Fi I2C."""

from host.python.i2c.protocol import PAYLOAD_SIZE, SLOT_SIZE, decode_slot, encode_slots


def main() -> int:
    payload = bytes(range(64))
    slots = encode_slots(payload, kind=0x01, transfer_id=1)
    rebuilt = bytearray()

    print(f"Logical payload: {len(payload)} bytes")
    print(f"Slot size: {SLOT_SIZE} bytes")
    print(f"Payload per slot: {PAYLOAD_SIZE} bytes")
    print(f"Slots: {len(slots)}")

    for index, raw in enumerate(slots, start=1):
        slot = decode_slot(raw)
        assert slot is not None
        rebuilt.extend(slot.data)
        print(
            f"slot {index}: len={len(raw)} offset={slot.offset} "
            f"data={len(slot.data)} flags=0x{slot.flags:02x}"
        )

    assert all(len(slot) == SLOT_SIZE for slot in slots)
    assert bytes(rebuilt) == payload
    print("I2C slot chunking test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
