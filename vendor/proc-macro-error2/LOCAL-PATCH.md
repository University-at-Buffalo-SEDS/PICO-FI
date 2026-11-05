# Local Rust compatibility patch

Upstream: proc-macro-error2 2.0.1, https://github.com/GnomedDev/proc-macro-error-2
Registry package checksum: 11ec05c52be0a07b08061f7dd003e7d7092e0472bc731b4af7bb1ef876109802
License: MIT OR Apache-2.0; both upstream license texts are retained.

The only upstream source change is `pub extern crate proc_macro;` in src/lib.rs.
The existing public __export module reexports this crate; making the original
extern crate declaration public fixes Rust E0365 / rust-lang/rust#127909 rather
than suppressing the warning. No macro logic, diagnostics or dependency versions
change. The upstream tests and package metadata are retained unchanged.

PIO 0.3 / pio-proc 0.3 is still required by embassy-rp 0.10. Upgrading only the
direct PIO dependency to 0.4 would leave this transitive dependency in place.
proc-macro-error2 has no newer released 2.x fix as checked on 2026-11-05.
Remove this patch when a compatible released dependency resolves the warning.

References:
- https://docs.rs/crate/proc-macro-error2/latest/source/src/lib.rs
- https://docs.rs/crate/pio-proc/0.3.0/source/Cargo.toml
- https://github.com/rust-lang/rust/issues/127909
