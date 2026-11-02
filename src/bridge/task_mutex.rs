//! Cross-core UART mailbox lock. Only task code may enter this lock.
//!
//! The payload copy must not hold the RP2040 global interrupt critical section.
//! Portable atomic acquisition/release may briefly enter one on ARMv6-M, but
//! interrupts remain enabled throughout the queue operation itself. Never await,
//! perform I/O, or recursively acquire this lock from its closure.

use embassy_sync::blocking_mutex::raw::RawMutex;
use portable_atomic::{AtomicBool, Ordering};

pub struct TaskSpinRawMutex {
    held: AtomicBool,
}

struct Release<'a>(&'a AtomicBool);
impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// SAFETY: A successful acquire is the sole way to enter the closure. The
// acquire/release pair synchronizes all protected data between both cores.
// The guard also releases ownership on unwind in host tests. No IRQ handler
// may acquire this task-only lock or interrupt a task and wait for its owner.
unsafe impl RawMutex for TaskSpinRawMutex {
    const INIT: Self = Self {
        held: AtomicBool::new(false),
    };

    fn lock<R>(&self, f: impl FnOnce() -> R) -> R {
        #[cfg(target_arch = "arm")]
        {
            assert!(
                matches!(
                    cortex_m::peripheral::SCB::vect_active(),
                    cortex_m::peripheral::scb::VectActive::ThreadMode
                ),
                "UART mailbox lock requires task context"
            );
            assert!(
                cortex_m::register::primask::read().is_active(),
                "UART mailbox lock requires enabled interrupts"
            );
        }
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        let _release = Release(&self.held);
        f()
    }
}
