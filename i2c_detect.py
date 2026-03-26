#!/usr/bin/env python3
"""Quick I2C address detection and validation"""
import sys

from i2c_raw import open_bus

bus_num = 1
addr = 0x55

try:
    bus = open_bus(bus_num)

    try:
        data = bus.read(addr, 1)[0]
        print(f"✅ Device at 0x{addr:02x} responded with byte: 0x{data:02x}")
    except Exception as e:
        print(f"❌ Device at 0x{addr:02x} not responding: {e}")
        print("\nTroubleshooting:")
        print("1. Check GPIO0 (SDA) and GPIO1 (SCL) are wired correctly")
        print("2. Verify Pico is powered on")
        print("3. Check pull-up resistors (4.7k ohm recommended)")
        print("4. Run: i2cdetect -y 1")
    
    bus.close()
    
except Exception as e:
    print(f"ERROR opening I2C bus {bus_num}: {e}")
    print("Make sure I2C is enabled")
    sys.exit(1)
