//! NDJSON frames for the SSH stdio carrier (aligned with `remuda-node` `StdioCarrier`).
//!
//! Each UTF-8 line is one JSON value. Cap is `protocol.md` §7.4
//! `maxJsonFrameBytes` (1 MiB). WebSocket uses one message per JSON value;
//! both implement [`crate::NodeTransport`].

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::Error;

/// `protocol.md` §7.4 suggested `maxJsonFrameBytes`.
pub const MAX_JSON_FRAME_BYTES: u32 = 1_048_576;

/// Encode `value` as compact JSON plus a trailing newline.
pub fn encode_json_frame(value: &Value, max_bytes: u32) -> Result<Vec<u8>, Error> {
    let mut payload = serde_json::to_vec(value)?;
    let len = u32::try_from(payload.len()).map_err(|_| Error::FrameTooLarge {
        len: u32::MAX,
        max: max_bytes,
    })?;
    if len == 0 {
        return Err(Error::EmptyFrame);
    }
    if len > max_bytes {
        return Err(Error::FrameTooLarge {
            len,
            max: max_bytes,
        });
    }
    payload.push(b'\n');
    Ok(payload)
}

/// Write one NDJSON frame and flush.
pub async fn write_json_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
    max_bytes: u32,
) -> Result<(), Error> {
    let buf = encode_json_frame(value, max_bytes)?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one NDJSON frame. `Ok(None)` is a clean EOF before any bytes.
/// Empty lines are skipped.
pub async fn read_json_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: u32,
) -> Result<Option<Value>, Error> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let len = u32::try_from(line.len()).unwrap_or(u32::MAX);
        if len > max_bytes {
            return Err(Error::FrameTooLarge {
                len,
                max: max_bytes,
            });
        }
        if !line.ends_with('\n') {
            return Err(Error::TruncatedFrame);
        }
        let line = line.trim_end_matches(['\n', '\r']).trim();
        if line.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(line)?));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[test]
    fn round_trip_bytes() {
        let value = json!({"method":"node.hello","jsonrpc":"2.0"});
        let encoded = encode_json_frame(&value, MAX_JSON_FRAME_BYTES).unwrap();
        assert!(encoded.ends_with(b"\n"));
        let parsed: Value = serde_json::from_slice(&encoded[..encoded.len() - 1]).unwrap();
        assert_eq!(parsed, value);
    }

    #[test]
    fn rejects_oversize() {
        let value = json!("hello");
        let err = encode_json_frame(&value, 1).unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn eof_before_frame_is_none() {
        let mut cursor = BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));
        assert_eq!(
            read_json_frame(&mut cursor, MAX_JSON_FRAME_BYTES)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn truncated_line_is_error() {
        let mut cursor = BufReader::new(std::io::Cursor::new(b"{\"method\":\"x\""));
        let err = read_json_frame(&mut cursor, MAX_JSON_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::TruncatedFrame));
    }
}
