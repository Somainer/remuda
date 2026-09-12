//! NDJSON line codec for Claude stream-json.

use crate::error::Error;
use crate::types::{Inbound, Outbound};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Default maximum NDJSON line size (8 MiB). Paseo uses the same ceiling.
pub const DEFAULT_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Encode `value` as one NDJSON line (no CR, trailing `\n`).
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Write one NDJSON line and flush.
pub async fn write_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), Error> {
    let buf = encode_line(value)?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// Read the next non-empty NDJSON line as UTF-8 bytes.
///
/// Empty lines and lone CR/LF (including the Windows CRLF bug Claude fixed in
/// 2.1.x) are skipped. Returns `Ok(None)` on EOF with no partial line.
pub async fn read_raw_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<Option<Vec<u8>>, Error> {
    loop {
        let mut line = Vec::new();
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                break;
            }
            if let Some(index) = available.iter().position(|&b| b == b'\n') {
                let add = index;
                if line.len().saturating_add(add) > max_line_bytes {
                    return Err(Error::LineTooLong {
                        limit: max_line_bytes,
                    });
                }
                line.extend_from_slice(&available[..add]);
                reader.consume(index + 1);
                break;
            }
            if line.len().saturating_add(available.len()) > max_line_bytes {
                return Err(Error::LineTooLong {
                    limit: max_line_bytes,
                });
            }
            let consume = available.len();
            line.extend_from_slice(available);
            reader.consume(consume);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        return Ok(Some(line));
    }
}

/// Read the next non-empty line as a JSON value.
pub async fn read_value<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<Option<Value>, Error> {
    match read_raw_line(reader, max_line_bytes).await? {
        None => Ok(None),
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
    }
}

/// Read the next stdout frame.
pub async fn read_outbound<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<Option<Outbound>, Error> {
    Ok(read_value(reader, max_line_bytes)
        .await?
        .map(Outbound::from_value))
}

/// Read the next stdin frame (used by tests and fixtures).
pub async fn read_inbound<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<Option<Inbound>, Error> {
    Ok(read_value(reader, max_line_bytes)
        .await?
        .map(Inbound::from_value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Inbound, PermissionResult};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn skips_empty_and_crlf_lines() {
        let data = b"\n\r\n{\"type\":\"keep_alive\"}\r\n\n{\"type\":\"keep_alive\"}\n";
        let mut reader = BufReader::new(&data[..]);
        let first = read_outbound(&mut reader, DEFAULT_MAX_LINE_BYTES)
            .await
            .expect("read")
            .expect("line");
        assert!(matches!(first, Outbound::KeepAlive));
        let second = read_outbound(&mut reader, DEFAULT_MAX_LINE_BYTES)
            .await
            .expect("read")
            .expect("line");
        assert!(matches!(second, Outbound::KeepAlive));
        assert!(
            read_outbound(&mut reader, DEFAULT_MAX_LINE_BYTES)
                .await
                .expect("eof")
                .is_none()
        );
    }

    #[tokio::test]
    async fn rejects_overlong_lines() {
        let data = b"{\"type\":\"keep_alive\"}\n";
        let mut reader = BufReader::new(&data[..]);
        let err = read_outbound(&mut reader, 4).await.expect_err("limit");
        assert!(matches!(err, Error::LineTooLong { limit: 4 }));
    }

    #[tokio::test]
    async fn writes_permission_allow_camel_case() {
        let inbound = Inbound::control_success(
            "req-1",
            crate::types::ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input: serde_json::json!({"command": "true"}),
                updated_permissions: None,
            }),
        );
        let encoded = String::from_utf8(encode_line(&inbound).expect("enc")).expect("utf8");
        assert!(encoded.contains("\"updatedInput\""));
        assert!(!encoded.contains("updated_input"));
        assert!(encoded.ends_with('\n'));
        assert!(!encoded.contains('\r'));
    }
}
