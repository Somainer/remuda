//! Protocol v1 TTY binary framing.

use crate::NodeError;
use remuda_protocol::Id;
use uuid::Uuid;

/// Number of bytes in the protocol v1 binary frame header.
pub const TTY_FRAME_HEADER_BYTES: usize = 32;
/// Binary channel value for TTY output.
pub const TTY_CHANNEL_OUTPUT: u8 = 1;

/// Encode one protocol v1 TTY output frame.
pub fn encode_tty_frame(stream_id: &Id, offset: u64, payload: &[u8]) -> Result<Vec<u8>, NodeError> {
    let (_, uuid_text) = stream_id
        .as_str()
        .split_once('_')
        .ok_or_else(|| NodeError::InvalidRequest("TTY stream ID has no prefix".to_owned()))?;
    let uuid = Uuid::parse_str(uuid_text)
        .map_err(|error| NodeError::InvalidRequest(format!("invalid TTY stream UUID: {error}")))?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| NodeError::InvalidRequest("TTY payload exceeds u32 framing".to_owned()))?;

    let mut frame = Vec::with_capacity(TTY_FRAME_HEADER_BYTES + payload.len());
    frame.push(1);
    frame.push(TTY_CHANNEL_OUTPUT);
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(uuid.as_bytes());
    frame.extend_from_slice(&offset.to_be_bytes());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_protocol_v1_header() {
        let stream = Id::new("tty").expect("registered ID prefix");
        let frame = encode_tty_frame(&stream, 7, b"abc").expect("valid frame");
        assert_eq!(frame.len(), 35);
        assert_eq!(&frame[..4], &[1, TTY_CHANNEL_OUTPUT, 0, 0]);
        assert_eq!(&frame[20..28], &7_u64.to_be_bytes());
        assert_eq!(&frame[28..32], &3_u32.to_be_bytes());
        assert_eq!(&frame[32..], b"abc");
    }
}
