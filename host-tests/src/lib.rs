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

#[path = "../../src/protocol/discovery.rs"]
pub mod discovery;

#[path = "../../src/protocol/ipv4ll.rs"]
pub mod ipv4ll;

#[path = "../../src/protocol/dhcp_probe.rs"]
pub mod dhcp_probe;

mod protocol {
    pub use crate::{ipv4ll, dhcp_probe, stream};
}
#[path = "../../src/arp_guard.rs"]
pub mod arp_guard;

#[path = "../../src/config.rs"]
pub mod config;
#[cfg(test)]
#[path = "../../src/config_record.rs"]
pub mod config_record;

#[path = "../../src/protocol/auto_address.rs"]
pub mod auto_address;

mod bridge { pub use crate::overwrite_queue; }
#[path = "../../src/bridge/uart_mailbox.rs"]
pub mod uart_mailbox;
