//! Shared SPI transport diagnostics for field debugging.

use heapless::String;
use portable_atomic::{AtomicU32, AtomicU8, Ordering};

const KIND_UNKNOWN: u8 = 0;
const KIND_IDLE: u8 = 1;
const KIND_PARTIAL: u8 = 2;
const KIND_COMPLETE: u8 = 3;
const KIND_RESET: u8 = 4;

static LAST_KIND: AtomicU8 = AtomicU8::new(KIND_UNKNOWN);
static LAST_RECEIVED: AtomicU32 = AtomicU32::new(0);
static LAST_EXPECTED: AtomicU32 = AtomicU32::new(0);
static LAST_PREVIEW0: AtomicU8 = AtomicU8::new(0);
static LAST_PREVIEW1: AtomicU8 = AtomicU8::new(0);
static LAST_PREVIEW2: AtomicU8 = AtomicU8::new(0);
static LAST_PREVIEW3: AtomicU8 = AtomicU8::new(0);
static LAST_FIRST_BYTE: AtomicU8 = AtomicU8::new(0);
static LAST_WRITTEN: AtomicU32 = AtomicU32::new(0);
static LAST_BITS: AtomicU32 = AtomicU32::new(0);
static LAST_STAGED_MAGIC: AtomicU8 = AtomicU8::new(0);
static LAST_STAGED_LEN: AtomicU8 = AtomicU8::new(0);
static RESET_COUNT: AtomicU32 = AtomicU32::new(0);
static QUEUED_RESPONSE_COUNT: AtomicU32 = AtomicU32::new(0);
static LAST_QUEUED_MAGIC: AtomicU8 = AtomicU8::new(0);
static LAST_QUEUED_LEN: AtomicU8 = AtomicU8::new(0);
static LAST_STATUS_BEFORE: AtomicU8 = AtomicU8::new(0);
static LAST_STATUS_AFTER: AtomicU8 = AtomicU8::new(0);
static LAST_TRANSFER_OK: AtomicU8 = AtomicU8::new(0);
static TRANSFER_ERROR_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn record_transaction(
    kind: u8,
    received: usize,
    expected: usize,
    preview: &[u8],
    first_byte: u8,
    written: usize,
    bits: usize,
    staged_magic: u8,
    staged_len: u8,
) {
    LAST_KIND.store(kind, Ordering::Relaxed);
    LAST_RECEIVED.store(received.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    LAST_EXPECTED.store(expected.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    LAST_PREVIEW0.store(*preview.first().unwrap_or(&0), Ordering::Relaxed);
    LAST_PREVIEW1.store(*preview.get(1).unwrap_or(&0), Ordering::Relaxed);
    LAST_PREVIEW2.store(*preview.get(2).unwrap_or(&0), Ordering::Relaxed);
    LAST_PREVIEW3.store(*preview.get(3).unwrap_or(&0), Ordering::Relaxed);
    LAST_FIRST_BYTE.store(first_byte, Ordering::Relaxed);
    LAST_WRITTEN.store(written.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    LAST_BITS.store(bits.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    LAST_STAGED_MAGIC.store(staged_magic, Ordering::Relaxed);
    LAST_STAGED_LEN.store(staged_len, Ordering::Relaxed);
}

pub fn record_transfer_status(status_before: u8, status_after: u8, ok: bool) {
    LAST_STATUS_BEFORE.store(status_before, Ordering::Relaxed);
    LAST_STATUS_AFTER.store(status_after, Ordering::Relaxed);
    LAST_TRANSFER_OK.store(ok as u8, Ordering::Relaxed);
    if !ok {
        let _ = TRANSFER_ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn record_queued_response(magic: u8, len: u8) {
    let _ = QUEUED_RESPONSE_COUNT.fetch_add(1, Ordering::Relaxed);
    LAST_QUEUED_MAGIC.store(magic, Ordering::Relaxed);
    LAST_QUEUED_LEN.store(len, Ordering::Relaxed);
}

pub fn render_snapshot() -> String<160> {
    let mut out = String::<160>::new();
    let kind = match LAST_KIND.load(Ordering::Relaxed) {
        KIND_IDLE => "idle",
        KIND_PARTIAL => "partial",
        KIND_COMPLETE => "complete",
        KIND_RESET => "reset",
        _ => "unknown",
    };
    let received = LAST_RECEIVED.load(Ordering::Relaxed);
    let expected = LAST_EXPECTED.load(Ordering::Relaxed);
    let preview0 = LAST_PREVIEW0.load(Ordering::Relaxed);
    let preview1 = LAST_PREVIEW1.load(Ordering::Relaxed);
    let preview2 = LAST_PREVIEW2.load(Ordering::Relaxed);
    let preview3 = LAST_PREVIEW3.load(Ordering::Relaxed);
    let first_byte = LAST_FIRST_BYTE.load(Ordering::Relaxed);
    let written = LAST_WRITTEN.load(Ordering::Relaxed);
    let bits = LAST_BITS.load(Ordering::Relaxed);
    let staged_magic = LAST_STAGED_MAGIC.load(Ordering::Relaxed);
    let staged_len = LAST_STAGED_LEN.load(Ordering::Relaxed);
    let resets = RESET_COUNT.load(Ordering::Relaxed);
    let queued = QUEUED_RESPONSE_COUNT.load(Ordering::Relaxed);
    let queued_magic = LAST_QUEUED_MAGIC.load(Ordering::Relaxed);
    let queued_len = LAST_QUEUED_LEN.load(Ordering::Relaxed);
    let status_before = LAST_STATUS_BEFORE.load(Ordering::Relaxed);
    let status_after = LAST_STATUS_AFTER.load(Ordering::Relaxed);
    let transfer_ok = LAST_TRANSFER_OK.load(Ordering::Relaxed);
    let transfer_errors = TRANSFER_ERROR_COUNT.load(Ordering::Relaxed);
    let _ = core::fmt::write(
        &mut out,
        format_args!(
            "spi kind={kind} r={received} e={expected} fb={first_byte:02x} rx={preview0:02x} {preview1:02x} {preview2:02x} {preview3:02x} w={written} b={bits} tx={staged_magic:02x}/{staged_len} q={queued}:{queued_magic:02x}/{queued_len} xfer={transfer_ok} sr={status_before:02x}/{status_after:02x} xerr={transfer_errors} resets={resets}"
        ),
    );
    out
}

pub const fn idle_kind() -> u8 {
    KIND_IDLE
}

pub const fn partial_kind() -> u8 {
    KIND_PARTIAL
}

pub const fn complete_kind() -> u8 {
    KIND_COMPLETE
}
