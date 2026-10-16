#![deny(unsafe_code)]

#[path = "../../src/protocol/stream.rs"]
pub mod stream;

#[path = "../../src/protocol/i2c_packet.rs"]
pub mod i2c_packet;

// Production queue uses Embassy's critical-section lock_mut API.
#[allow(unsafe_code)]
#[path = "../../src/bridge/overwrite_queue.rs"]
pub mod overwrite_queue;

#[path = "../../src/protocol/duplex.rs"]
pub mod duplex;
