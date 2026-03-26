#!/usr/bin/env python3
"""
Comprehensive SPI test for the pico-fi response fix
Run this on the Raspberry Pi to test the updated firmware
"""

import subprocess
import time
import sys

def run_test(name, cmd):
    """Run a test command and capture output"""
    print(f"\n{'='*70}")
    print(f"TEST: {name}")
    print(f"{'='*70}")
    print(f"Command: {cmd}\n")
    
    try:
        result = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10)
        print(result.stdout)
        if result.stderr:
            print("STDERR:", result.stderr)
        return result.returncode == 0
    except subprocess.TimeoutExpired:
        print("TIMEOUT: Test took too long")
        return False
    except Exception as e:
        print(f"ERROR: {e}")
        return False

def main():
    print("\n" + "="*70)
    print("PICO-FI SPI RESPONSE FIX - COMPREHENSIVE TEST SUITE")
    print("="*70)
    print("\nThis test verifies that SPI command responses work correctly")
    print("with proper text payloads (not 0xFF garbage)\n")
    
    # Wait for Pico to stabilize after flash
    print("Waiting 3 seconds for Pico to stabilize...")
    time.sleep(3)
    
    results = {}
    
    # Test 1: /ping command
    results['ping'] = run_test(
        "/ping command",
        "python3 spi_test.py command /ping"
    )
    time.sleep(0.5)
    
    # Test 2: /link command
    results['link'] = run_test(
        "/link command",
        "python3 spi_test.py command /link"
    )
    time.sleep(0.5)
    
    # Test 3: /help command
    results['help'] = run_test(
        "/help command",
        "python3 spi_test.py command /help"
    )
    time.sleep(0.5)
    
    # Test 4: Probe frames
    results['probe'] = run_test(
        "Probe 10 frames",
        "python3 spi_test.py probe --count 10"
    )
    
    # Print summary
    print("\n" + "="*70)
    print("TEST SUMMARY")
    print("="*70)
    
    passed = sum(1 for v in results.values() if v)
    total = len(results)
    
    for test_name, result in results.items():
        status = "✅ PASS" if result else "❌ FAIL"
        print(f"{test_name:15s}: {status}")
    
    print(f"\nTotal: {passed}/{total} tests passed")
    
    if passed == total:
        print("\n🎉 ALL TESTS PASSED - Response fix is working! 🎉")
        return 0
    else:
        print(f"\n⚠️  {total - passed} test(s) failed - see details above")
        return 1

if __name__ == "__main__":
    sys.exit(main())

