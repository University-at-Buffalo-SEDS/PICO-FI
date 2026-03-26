#!/usr/bin/env python3
"""Quick I2C address detection and validation."""

import sys

from .raw import open_bus

BUS_NUM = 1
ADDR = 0x55


def main() -> int:
    try:
        bus = open_bus(BUS_NUM)
    except Exception as exc:
        print(f"ERROR opening I2C bus {BUS_NUM}: {exc}")
        print("Make sure I2C is enabled")
        return 1

    try:
        data = bus.read(ADDR, 1)[0]
        print(f"✅ Device at 0x{ADDR:02x} responded with byte: 0x{data:02x}")
        return 0
    except Exception as exc:
        print(f"❌ Device at 0x{ADDR:02x} not responding: {exc}")
        print("\nTroubleshooting:")
        print("1. Check GPIO0 (SDA) and GPIO1 (SCL) are wired correctly")
        print("2. Verify Pico is powered on")
        print("3. Check pull-up resistors (4.7k ohm recommended)")
        print("4. Run: i2cdetect -y 1")
        return 1
    finally:
        bus.close()


if __name__ == "__main__":
    sys.exit(main())
