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
    let len = frame[1] as usize;
    if len > PAYLOAD_MAX {
        return None;
    }

    let payload = &frame[2..2 + len];
    match frame[0] {
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

