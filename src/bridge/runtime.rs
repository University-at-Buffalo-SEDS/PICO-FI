//! Shared runtime parameters for bridge role implementations.

use portable_atomic::AtomicBool;

/// Shared runtime knobs and state references used by bridge implementations.
#[derive(Clone, Copy)]
pub struct BridgeRuntime<'a> {
    /// Shared link-state flag consumed by commands and status indication.
    pub link_active: &'a AtomicBool,
    /// Delay before the client role attempts its first outbound TCP connection.
    pub startup_delay_ms: u64,
    /// Delay between failed or closed client reconnect attempts.
    pub reconnect_delay_ms: u64,
    /// Timeout applied to outbound TCP connection establishment.
    pub connect_timeout_ms: u64,
    /// Timeout applied to the bridge handshake exchange after TCP connects.
    pub handshake_timeout_ms: u64,
    /// Maximum idle time on an established TCP session before the link is dropped.
    pub session_timeout_ms: u64,
    /// Fixed magic exchanged by both peers to confirm protocol compatibility.
    pub handshake_magic: &'a [u8],
}
