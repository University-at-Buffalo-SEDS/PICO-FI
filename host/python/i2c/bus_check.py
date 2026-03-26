#!/usr/bin/env python3
"""Test I2C bus status and Pico connectivity."""

import subprocess


def main() -> int:
    print("I2C Bus Status Check")
    print("=" * 50)
    result = subprocess.run("ls /dev/i2c*", capture_output=True, text=True, shell=True)
    print(f"I2C devices: {result.stdout}")
    print("\nI2C bus 1 scan:")
    result = subprocess.run(["i2cdetect", "-y", "1"], capture_output=True, text=True)
    print(result.stdout)
    print("\nScanning all I2C buses (0-3):")
    for bus in range(4):
        print(f"\nBus {bus}:")
        result = subprocess.run(["i2cdetect", "-y", str(bus)], capture_output=True, text=True)
        lines = result.stdout.strip().split("\n")
        for line in lines:
            if any(c.isalnum() for c in line) and not line.startswith("     "):
                print(line)
    print("\nPossible issues:")
    print("1. GPIO0/1 not configured as I2C in firmware")
    print("2. I2C bus not enabled on Pi5")
    print("3. Pico not powered or not flashed with latest firmware")
    print("4. Wiring issue - check GPIO0↔SDA, GPIO1↔SCL connections")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
