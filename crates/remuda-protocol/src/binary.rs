//! TTY/object binary framing, independent of transport and authorization; `protocol.md` §7.4.

use crate::{U64, WireValueError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::{Uuid, Variant};

/// Fixed prefix length for negotiated `tty-binary-v1` frames; §7.4.
pub const BINARY_HEADER_LEN: usize = 32;

/// Output stream category; the byte values are fixed by `protocol.md` §7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum BinaryChannel {
    /// Terminal output (ANSI / PTY bytes).
    TtyOutput = 1,
    /// Object transfer chunk.
    ObjectChunk = 2,
    /// Terminal input: raw bytes as the client emitted them (keyboard and mouse).
    TtyInput = 3,
}

/// Canonical UUIDv7 portion of a registered stream ID; `protocol.md` §7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StreamUuid(Uuid);

impl TryFrom<String> for StreamUuid {
    type Error = WireValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let uuid =
            Uuid::parse_str(&value).map_err(|_| WireValueError("invalid stream UUID".into()))?;
        if uuid.get_version_num() != 7
            || uuid.get_variant() != Variant::RFC4122
            || uuid.to_string() != value
        {
            return Err(WireValueError("stream requires canonical UUIDv7".into()));
        }
        Ok(Self(uuid))
    }
}
impl From<StreamUuid> for String {
    fn from(value: StreamUuid) -> Self {
        value.0.to_string()
    }
}

impl StreamUuid {
    /// UUID bytes carried in the binary header (no `tty_` prefix).
    #[must_use]
    pub fn uuid(self) -> Uuid {
        self.0
    }

    /// Strip a registered `tty_…` (or other) ID down to the UUIDv7 header field.
    pub fn from_prefixed_id(id: &str) -> Result<Self, BinaryFrameError> {
        let uuid_text = id.split_once('_').map(|(_, rest)| rest).unwrap_or(id);
        uuid_text
            .to_owned()
            .try_into()
            .map_err(|_| BinaryFrameError::InvalidStream)
    }
}
impl JsonSchema for StreamUuid {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StreamUuid".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string", "pattern":"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"})
    }
}

/// Decoded metadata for the 32-byte binary header; JSON is for fixtures only; §7.4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BinaryHeader {
    /// Byte 1; bytes 2 and 3 are always zero.
    pub channel: BinaryChannel,
    /// Bytes 4–19, without the registry ID prefix.
    pub stream_uuid: StreamUuid,
    /// Bytes 20–27, big-endian byte offset.
    pub offset: U64,
    /// Bytes 28–31, big-endian length of the following payload.
    pub payload_length: u32,
}

/// Rejection of a binary frame before routing or allocation; `protocol.md` §7.4.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BinaryFrameError {
    /// Less than a complete header was received.
    #[error("truncated binary header")]
    Truncated,
    /// Framing version, channel, or reserved bytes do not match v1.
    #[error("unsupported binary framing version, channel, or reserved bytes")]
    UnsupportedHeader,
    /// Stream UUID is not a valid UUIDv7.
    #[error("invalid binary stream UUID")]
    InvalidStream,
    /// Declared payload length does not equal the received length.
    #[error("binary payload length mismatch")]
    LengthMismatch,
    /// Negotiated per-frame payload maximum would be exceeded.
    #[error("binary payload exceeds negotiated limit")]
    PayloadLimit,
    /// Byte offset plus payload length overflows U64.
    #[error("binary stream offset overflow")]
    OffsetOverflow,
}

impl BinaryHeader {
    /// Encode a v1 header after checking the represented byte range; §7.4.
    pub fn encode(&self) -> Result<[u8; BINARY_HEADER_LEN], BinaryFrameError> {
        self.offset
            .0
            .checked_add(u64::from(self.payload_length))
            .ok_or(BinaryFrameError::OffsetOverflow)?;
        let mut bytes = [0; BINARY_HEADER_LEN];
        bytes[0] = 1;
        bytes[1] = self.channel as u8;
        bytes[4..20].copy_from_slice(self.stream_uuid.0.as_bytes());
        bytes[20..28].copy_from_slice(&self.offset.0.to_be_bytes());
        bytes[28..32].copy_from_slice(&self.payload_length.to_be_bytes());
        Ok(bytes)
    }

    /// Encode header plus payload as one frame.
    pub fn encode_frame(&self, payload: &[u8]) -> Result<Vec<u8>, BinaryFrameError> {
        if payload.len() as u64 != u64::from(self.payload_length) {
            return Err(BinaryFrameError::LengthMismatch);
        }
        let mut frame = Vec::with_capacity(BINARY_HEADER_LEN + payload.len());
        frame.extend_from_slice(&self.encode()?);
        frame.extend_from_slice(payload);
        Ok(frame)
    }
}

/// Encode one v1 binary frame; `protocol.md` §7.4.
pub fn encode_binary_frame(
    channel: BinaryChannel,
    stream_uuid: StreamUuid,
    offset: u64,
    payload: &[u8],
) -> Result<Vec<u8>, BinaryFrameError> {
    let payload_length =
        u32::try_from(payload.len()).map_err(|_| BinaryFrameError::PayloadLimit)?;
    BinaryHeader {
        channel,
        stream_uuid,
        offset: U64(offset),
        payload_length,
    }
    .encode_frame(payload)
}

/// Decode one complete frame without allocating payload bytes; `protocol.md` §7.4.
///
/// The caller must additionally resolve the UUID to an authorized registered stream,
/// verify its epoch, and check offset continuity. This function never grants input rights.
pub fn decode_binary_frame(
    frame: &[u8],
    max_payload: u32,
) -> Result<(BinaryHeader, &[u8]), BinaryFrameError> {
    let prefix: &[u8; BINARY_HEADER_LEN] = frame
        .get(..BINARY_HEADER_LEN)
        .ok_or(BinaryFrameError::Truncated)?
        .try_into()
        .map_err(|_| BinaryFrameError::Truncated)?;
    if prefix[0] != 1 || prefix[2] != 0 || prefix[3] != 0 {
        return Err(BinaryFrameError::UnsupportedHeader);
    }
    let channel = match prefix[1] {
        1 => BinaryChannel::TtyOutput,
        2 => BinaryChannel::ObjectChunk,
        3 => BinaryChannel::TtyInput,
        _ => return Err(BinaryFrameError::UnsupportedHeader),
    };
    let uuid = Uuid::from_slice(&prefix[4..20]).map_err(|_| BinaryFrameError::InvalidStream)?;
    let stream_uuid = uuid
        .to_string()
        .try_into()
        .map_err(|_| BinaryFrameError::InvalidStream)?;
    let offset = u64::from_be_bytes(
        prefix[20..28]
            .try_into()
            .map_err(|_| BinaryFrameError::Truncated)?,
    );
    let length = u32::from_be_bytes(
        prefix[28..32]
            .try_into()
            .map_err(|_| BinaryFrameError::Truncated)?,
    );
    if length > max_payload {
        return Err(BinaryFrameError::PayloadLimit);
    }
    let payload = &frame[BINARY_HEADER_LEN..];
    if payload.len() as u64 != u64::from(length) {
        return Err(BinaryFrameError::LengthMismatch);
    }
    offset
        .checked_add(u64::from(length))
        .ok_or(BinaryFrameError::OffsetOverflow)?;
    Ok((
        BinaryHeader {
            channel,
            stream_uuid,
            offset: U64(offset),
            payload_length: length,
        },
        payload,
    ))
}
