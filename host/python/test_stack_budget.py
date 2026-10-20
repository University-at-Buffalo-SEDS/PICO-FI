import importlib.util
from pathlib import Path
import unittest

path = Path(__file__).resolve().parents[2] / "tools/check_stack_budget.py"
spec = importlib.util.spec_from_file_location("stack_budget", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class StackBudgetTests(unittest.TestCase):
    def test_observed_arm_uart_frame_rejects_old_16k_stack(self):
        with self.assertRaisesRegex(ValueError, "exceeds"):
            module.validate_frame(25448, 16384, "UART core 1")
        module.validate_frame(25448, 48 * 1024, "UART core 1")

    def test_reserve_is_not_allowed_to_fill_main_stack_completely(self):
        with self.assertRaises(ValueError):
            module.validate_frame(25000, 30000, "Ethernet core 0")

    def test_real_old_arm_prologue_counts_large_literal_stack_adjustment(self):
        # PUSH {r4,r5,r6,r7,lr}; ADD r7,sp,#12; LDR r6,[pc,#612]; ADD sp,r6
        old = bytes.fromhex('f0b503af994eb544')
        self.assertEqual(module.thumb_frame(old,0x100137d8,lambda _:0xffff9cac),25448)
        with self.assertRaises(ValueError):
            module.validate_frame(module.thumb_frame(old,0x100137d8,lambda _:0xffff9cac),16384,'UART')

    def test_small_frame_and_unknown_register_allocation(self):
        self.assertEqual(module.thumb_frame(bytes.fromhex('80b582b0'),0,lambda _:0),16)
        with self.assertRaisesRegex(ValueError,'unknown'):
            module.thumb_frame(bytes.fromhex('b544'),0,lambda _:0)
