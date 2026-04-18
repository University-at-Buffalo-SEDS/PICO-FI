//! Shared framed request and response definitions for UART-like upstream links.

/// Total number of bytes in a single fixed SPI frame transfer.
pub const FRAME_SIZE: usize = 260;

/// Maximum application payload that fits in one framed transfer.
pub const PAYLOAD_MAX: usize = FRAME_SIZE - FRAME_HEADER_SIZE;

/// Number of bytes in the common link frame header.
pub const FRAME_HEADER_SIZE: usize = 4;

/// Request magic for raw bridge data.
pub const REQ_DATA_MAGIC: u8 = 0xA5;

/// Request magic for local Pico commands.
pub const REQ_COMMAND_MAGIC: u8 = 0xA6;

/// Response magic for raw bridge data.
pub const RESP_DATA_MAGIC: u8 = 0x5A;

/// Response magic for local Pico command replies.
pub const RESP_COMMAND_MAGIC: u8 = 0x5B;

/// First header byte for raw ASCII frames consumed by the gateway board.
pub const REQ_RAW_ASCII_MAGIC: u8 = 0xA7;

/// Second header byte for raw ASCII frames consumed by the gateway board.
pub const RESP_RAW_ASCII_MAGIC: u8 = 0x7A;

/// Decoded I2C request classification.
pub enum RequestFrame<'a> {
    /// Payload bytes that should be forwarded across the Ethernet bridge unchanged.
    Data(&'a [u8]),
    /// ASCII command bytes that should be handled locally on the Pico.
    Command(&'a [u8]),
}

/// Parses a full I2C request frame into a typed request view.
pub fn parse_request_frame(frame: &[u8; FRAME_SIZE]) -> Option<RequestFrame<'_>> {
    parse_request_bytes(frame)
}

/// Parses a variable-length request frame into a typed request view.
pub fn parse_request_bytes(frame: &[u8]) -> Option<RequestFrame<'_>> {
    let (kind, len) = parse_frame_header(frame)?;
    if frame.len() < FRAME_HEADER_SIZE + len {
        return None;
    }

    let payload = &frame[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + len];
    match kind {
        FrameKind::Data => Some(RequestFrame::Data(payload)),
        FrameKind::Command => Some(RequestFrame::Command(payload)),
        FrameKind::RawAscii => None,
    }
}

/// Builds a response frame with the given magic byte and payload.
pub fn make_response_frame(magic: u8, payload: &[u8]) -> [u8; FRAME_SIZE] {
    let mut frame = [0u8; FRAME_SIZE];
    let len = payload.len().min(PAYLOAD_MAX);
    let (h0, h1) = header_for_magic(magic).unwrap_or((REQ_DATA_MAGIC, RESP_DATA_MAGIC));
    frame[0] = h0;
    frame[1] = h1;
    frame[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    frame[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + len].copy_from_slice(&payload[..len]);
    frame
}

/// Writes a variable-length frame into `out`, returning the number of bytes used.
pub fn build_frame_into(magic: u8, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let (h0, h1) = header_for_magic(magic)?;
    let max_payload = out.len().checked_sub(FRAME_HEADER_SIZE)?;
    let len = payload.len().min(max_payload).min(u16::MAX as usize);
    out[0] = h0;
    out[1] = h1;
    out[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    out[FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + len].copy_from_slice(&payload[..len]);
    Some(FRAME_HEADER_SIZE + len)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FrameKind {
    Data,
    Command,
    RawAscii,
}

fn parse_frame_header(frame: &[u8]) -> Option<(FrameKind, usize)> {
    if frame.len() < FRAME_HEADER_SIZE {
        return None;
    }
    let kind = match (frame[0], frame[1]) {
        (REQ_DATA_MAGIC, RESP_DATA_MAGIC) | (RESP_DATA_MAGIC, REQ_DATA_MAGIC) => FrameKind::Data,
        (REQ_COMMAND_MAGIC, RESP_COMMAND_MAGIC) | (RESP_COMMAND_MAGIC, REQ_COMMAND_MAGIC) => {
            FrameKind::Command
        }
        (REQ_RAW_ASCII_MAGIC, RESP_RAW_ASCII_MAGIC) => FrameKind::RawAscii,
        _ => return None,
    };
    let len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
    Some((kind, len))
}

fn header_for_magic(magic: u8) -> Option<(u8, u8)> {
    match magic {
        REQ_DATA_MAGIC | RESP_DATA_MAGIC => Some((REQ_DATA_MAGIC, RESP_DATA_MAGIC)),
        REQ_COMMAND_MAGIC | RESP_COMMAND_MAGIC => Some((REQ_COMMAND_MAGIC, RESP_COMMAND_MAGIC)),
        REQ_RAW_ASCII_MAGIC | RESP_RAW_ASCII_MAGIC => {
            Some((REQ_RAW_ASCII_MAGIC, RESP_RAW_ASCII_MAGIC))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_response_frame() {
        let payload = b"pong";
        let frame = make_response_frame(RESP_COMMAND_MAGIC, payload);
        assert_eq!(frame[0], REQ_COMMAND_MAGIC);
        assert_eq!(frame[1], RESP_COMMAND_MAGIC);
        assert_eq!(&frame[2..4], &4u16.to_le_bytes());
        assert_eq!(&frame[4..8], b"pong");
        assert_eq!(frame[8], 0);
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
