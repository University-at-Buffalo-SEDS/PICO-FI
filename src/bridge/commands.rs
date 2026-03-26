//! Local bridge command rendering shared by UART and I2C control paths.

use crate::config::{BridgeConfig, render_config};
use portable_atomic::{AtomicBool, Ordering};
use heapless::String;

/// Renders the response for a locally handled bridge command.
pub fn render_local_bridge_command(
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    line: &str,
) -> String<192> {
    let mut out = String::<192>::new();
    match line {
        "/help" => {
            let _ = out.push_str("pico commands: /help /show /ping /link");
        }
        "/show" => {
            let rendered = render_config(&bridge_config);
            let _ = out.push_str(rendered.as_str());
        }
        "/ping" => {
            let _ = out.push_str("pong");
        }
        "/link" => {
            if link_active.load(Ordering::Relaxed) {
                let _ = out.push_str("link up");
            } else {
                let _ = out.push_str("link down");
            }
        }
        _ => {
            let _ = out.push_str("error unknown pico command");
        }
    }
    out
}

/// Trims ASCII line endings and surrounding ASCII whitespace from a byte slice.
pub fn trim_ascii_line(buf: &[u8]) -> &str {
    let text = core::str::from_utf8(buf).unwrap_or_default();
    text.trim_matches(|ch| matches!(ch, '\r' | '\n' | ' ' | '\t'))
}
