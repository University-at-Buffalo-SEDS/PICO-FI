//! Framed I2C request and response definitions.

/// Total number of bytes in a single I2C frame transfer.
pub const FRAME_SIZE: usize = 258;

/// Maximum application payload that fits in one framed transfer.
pub const PAYLOAD_MAX: usize = FRAME_SIZE - 2;

/// Request magic for raw bridge data.
pub const REQ_DATA_MAGIC: u8 = 0xA5;

/// Request magic for local Pico commands.
pub const REQ_COMMAND_MAGIC: u8 = 0xA6;

/// Response magic for raw bridge data.
pub const RESP_DATA_MAGIC: u8 = 0x5A;

/// Response magic for local Pico command replies.
pub const RESP_COMMAND_MAGIC: u8 = 0x5B;

/// Decoded I2C request classification.
pub enum RequestFrame<'a> {
    /// Payload bytes that should be forwarded across the Ethernet bridge unchanged.
    Data(&'a [u8]),
    /// ASCII command bytes that should be handled locally on the Pico.
    Command(&'a [u8]),
}

/// Parses a full I2C request frame into a typed request view.
pub fn parse_request_frame(frame: &[u8; FRAME_SIZE]) -> Option<RequestFrame<'_>> {
    let magic = frame[0];
    let len = frame[1] as usize;
    if len > PAYLOAD_MAX {
        return None;
    }

    let payload = &frame[2..2 + len];
    match magic {
        REQ_DATA_MAGIC => Some(RequestFrame::Data(payload)),
        REQ_COMMAND_MAGIC => Some(RequestFrame::Command(payload)),
        _ => None,
    }
}

/// Builds a response frame with the given magic byte and payload.
pub fn make_response_frame(magic: u8, payload: &[u8]) -> [u8; FRAME_SIZE] {
    let mut frame = [0u8; FRAME_SIZE];
    let len = payload.len().min(PAYLOAD_MAX);
    frame[0] = magic;
    frame[1] = len as u8;
    frame[2..2 + len].copy_from_slice(&payload[..len]);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_response_frame() {
        let payload = b"pong";
        let frame = make_response_frame(RESP_COMMAND_MAGIC, payload);
        assert_eq!(frame[0], RESP_COMMAND_MAGIC);
        assert_eq!(frame[1], 4);
        assert_eq!(&frame[2..6], b"pong");
        assert_eq!(frame[6], 0);
    }

    #[test]
    fn parses_command_frame() {
        let frame = make_response_frame(REQ_COMMAND_MAGIC, b"/ping\n");
        match parse_request_frame(&frame) {
            Some(RequestFrame::Command(payload)) => assert_eq!(payload, b"/ping\n"),
            _ => panic!("command frame should parse"),
        }
    }
}
