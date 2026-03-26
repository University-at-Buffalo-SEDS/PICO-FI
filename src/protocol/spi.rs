//! Framed SPI request and response definitions.

/// Total number of bytes in a single SPI transaction.
pub const FRAME_SIZE: usize = 258;

/// Maximum application payload that fits in one framed SPI transfer.
pub const PAYLOAD_MAX: usize = FRAME_SIZE - 2;

/// Request magic for raw bridge data.
pub const REQ_DATA_MAGIC: u8 = 0xA5;

/// Request magic for local Pico commands.
pub const REQ_COMMAND_MAGIC: u8 = 0xA6;

/// Response magic for raw bridge data.
pub const RESP_DATA_MAGIC: u8 = 0x5A;

/// Response magic for local Pico command replies.
pub const RESP_COMMAND_MAGIC: u8 = 0x5B;

/// Decoded SPI request classification.
pub enum RequestFrame<'a> {
    /// Payload bytes that should be forwarded across the Ethernet bridge unchanged.
    Data(&'a [u8]),
    /// ASCII command bytes that should be handled locally on the Pico.
    Command(&'a [u8]),
}

/// Parses a full SPI request frame into a typed request view.
pub fn parse_request_frame(frame: &[u8; FRAME_SIZE]) -> Option<RequestFrame<'_>> {
    let magic = frame[0];
    let len = frame[1] as usize;

    if len > PAYLOAD_MAX {
        return None;
    }

    // Debug: if this is a command request, show what we got
    if magic == REQ_COMMAND_MAGIC {
        // Payload starts at frame[2]
        let payload = &frame[2..2 + len];
        if payload.len() > 0 {
            // Got some bytes
            if payload[0] == 0x2F {  // '/'
                // This is good - we have the slash
                return Some(RequestFrame::Command(payload));
            }
        }
        // Empty payload or no slash - something is wrong
        return Some(RequestFrame::Command(&[]));
    }

    if magic == REQ_DATA_MAGIC {
        return Some(RequestFrame::Data(&frame[2..2 + len]));
    }

    None
}

/// Builds a response frame with the given magic byte and payload.
pub fn make_response_frame(magic: u8, payload: &[u8]) -> [u8; FRAME_SIZE] {
    let mut frame = [0u8; FRAME_SIZE];

    // Ensure frame is completely zeroed
    for i in 0..FRAME_SIZE {
        frame[i] = 0;
    }

    // Set magic byte
    frame[0] = magic;

    // Calculate and set length
    let len = if payload.len() > PAYLOAD_MAX {
        PAYLOAD_MAX
    } else {
        payload.len()
    };
    frame[1] = len as u8;

    // Copy payload
    if len > 0 {
        for i in 0..len {
            frame[2 + i] = payload[i];
        }
    }

    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_response_frame() {
        let payload = b"pong";
        let frame = make_response_frame(RESP_COMMAND_MAGIC, payload);
        assert_eq!(frame[0], RESP_COMMAND_MAGIC);
        assert_eq!(frame[1], 4);
        assert_eq!(&frame[2..6], b"pong");
        assert_eq!(frame[6], 0);  // Rest should be zero
    }
}

