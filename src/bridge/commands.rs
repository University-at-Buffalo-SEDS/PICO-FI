//! Local bridge command rendering shared by UART and I2C control paths.

use crate::bridge::spi_diag;
use crate::config::{BridgeConfig, render_config};
use heapless::String;
use portable_atomic::{AtomicBool, AtomicU8, Ordering};

const LED_MODE_AUTO: u8 = 0;
const LED_MODE_OFF: u8 = 1;
const LED_MODE_ON: u8 = 2;
const LED_MODE_TOGGLE: u8 = 3;

static LED_COMMAND: AtomicU8 = AtomicU8::new(LED_MODE_AUTO);
static LED_STATE: AtomicU8 = AtomicU8::new(LED_MODE_OFF);
static LED_ACTIVITY_PULSES: AtomicU8 = AtomicU8::new(0);

pub fn take_led_command() -> Option<u8> {
    match LED_COMMAND.swap(LED_MODE_AUTO, Ordering::AcqRel) {
        LED_MODE_AUTO => None,
        cmd => Some(cmd),
    }
}

pub fn set_led_state(on: bool) {
    LED_STATE.store(if on { LED_MODE_ON } else { LED_MODE_OFF }, Ordering::Relaxed);
}

pub fn take_led_activity() -> bool {
    LED_ACTIVITY_PULSES.swap(0, Ordering::AcqRel) != 0
}

fn led_status_text() -> &'static str {
    match LED_STATE.load(Ordering::Relaxed) {
        LED_MODE_ON => "led on",
        _ => "led off",
    }
}

/// Renders the response for a locally handled bridge command.
pub fn render_local_bridge_command(
    bridge_config: BridgeConfig,
    link_active: &AtomicBool,
    line: &str,
) -> String<192> {
    let mut out = String::<192>::new();
    match line {
        "/help" => {
            let _ = out.push_str("pico commands: /help /show /ping /link /spi /led <on|off|toggle|auto|status>");
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
        "/spi" => {
            let rendered = spi_diag::render_snapshot();
            let _ = out.push_str(rendered.as_str());
        }
        "/led on" => {
            LED_COMMAND.store(LED_MODE_ON, Ordering::Release);
            let _ = out.push_str("ok led on");
        }
        "/led off" => {
            LED_COMMAND.store(LED_MODE_OFF, Ordering::Release);
            let _ = out.push_str("ok led off");
        }
        "/led toggle" => {
            LED_COMMAND.store(LED_MODE_TOGGLE, Ordering::Release);
            let _ = out.push_str("ok led toggle");
        }
        "/led auto" => {
            LED_COMMAND.store(LED_MODE_AUTO, Ordering::Release);
            let _ = out.push_str("ok led auto");
        }
        "/led status" => {
            let _ = out.push_str(led_status_text());
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
