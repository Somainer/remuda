//! NDJSON line codec for Codex app-server stdio.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::error::WireError;
use crate::rpc::WireFrame;

/// Skip empty lines, CR, and `#` comments (fixture headers).
pub fn is_skippable_line(line: &str) -> bool {
    let trimmed = line.trim_end_matches(['\n', '\r']).trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// Decode one NDJSON payload into a [`WireFrame`]. Non-object JSON becomes
/// [`WireFrame::Unknown`].
pub fn decode_line(line: &str) -> Result<WireFrame, WireError> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let value: serde_json::Value = serde_json::from_str(trimmed)?;
    Ok(WireFrame::from_value(value))
}

/// Read one line with a hard byte cap. `Ok(None)` is EOF.
pub async fn read_capped_line<R>(
    reader: &mut R,
    max_line_bytes: usize,
) -> Result<Option<Vec<u8>>, WireError>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let n = reader
        .read_until(b'\n', &mut buf)
        .await
        .map_err(WireError::Io)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > max_line_bytes {
        return Err(WireError::LineTooLong(max_line_bytes));
    }
    Ok(Some(buf))
}

/// UTF-8 lossy conversion that strips a trailing newline.
pub fn line_to_str(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    while text.ends_with(['\n', '\r']) {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::JsonRpcNotification;

    #[test]
    fn skips_comments_and_blank() {
        assert!(is_skippable_line("# source: probe"));
        assert!(is_skippable_line("\n"));
        assert!(is_skippable_line("\r\n"));
        assert!(!is_skippable_line("{\"method\":\"initialized\"}"));
    }

    #[test]
    fn decode_initialized() {
        match decode_line("{\"method\":\"initialized\"}\n").expect("decode") {
            WireFrame::Notification(JsonRpcNotification { method, params }) => {
                assert_eq!(method, "initialized");
                assert!(params.is_none());
            }
            other => panic!("{other:?}"),
        }
    }
}
