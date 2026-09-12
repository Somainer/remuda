//! Digests, timestamps, and `Knowledge` helpers.

use crate::Error;
use remuda_protocol::{Digest, EventId, Knowledge, Timestamp, U64};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

/// SHA-256 digest of `bytes` with the `sha256:` prefix.
pub fn digest_of(bytes: &[u8]) -> Digest {
    let hash = Sha256::digest(bytes);
    let mut out = String::from("sha256:");
    out.push_str(&hex_lower(&hash));
    match Digest::try_from(out) {
        Ok(digest) => digest,
        Err(_) => unreachable!("SHA-256 hex is always a valid Digest"),
    }
}

/// UTC timestamp with millisecond precision, as required by `protocol.md` §1.1.
pub fn timestamp_now() -> Result<Timestamp, Error> {
    timestamp_from_offset(OffsetDateTime::now_utc())
}

pub(crate) fn timestamp_from_offset(dt: OffsetDateTime) -> Result<Timestamp, Error> {
    let text = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    );
    Ok(Timestamp::try_from(text)?)
}

/// Parse a native RFC3339 timestamp into `Knowledge`, truncating extra fraction digits.
pub(crate) fn parse_timestamp(raw: &str) -> Knowledge<Timestamp> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return unknown("not-emitted");
    }
    let normalized = normalize_rfc3339_ms(trimmed);
    match Timestamp::try_from(normalized) {
        Ok(ts) => known(ts),
        Err(_) => unknown("unparsed-native-timestamp"),
    }
}

fn normalize_rfc3339_ms(raw: &str) -> String {
    let Some(dot) = raw.find('.') else {
        if raw.ends_with('Z') && raw.len() == 20 {
            return format!("{}.000Z", &raw[..19]);
        }
        return raw.to_owned();
    };
    let head = &raw[..=dot];
    let tail = &raw[dot + 1..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest: String = tail.chars().skip_while(|c| c.is_ascii_digit()).collect();
    let mut frac = digits;
    frac.truncate(3);
    while frac.len() < 3 {
        frac.push('0');
    }
    format!("{head}{frac}{rest}")
}

pub(crate) fn known<T>(value: T) -> Knowledge<T> {
    Knowledge::Known { value }
}

pub(crate) fn unknown<T>(reason: impl Into<String>) -> Knowledge<T> {
    Knowledge::Unknown {
        reason: reason.into(),
        evidence_event_ids: Vec::<EventId>::new(),
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn digest_hex(digest: &Digest) -> String {
    let text: String = digest.clone().into();
    text.trim_start_matches("sha256:").to_owned()
}

pub(crate) fn u64_i64(value: U64) -> Result<i64, Error> {
    i64::try_from(value.0).map_err(|_| Error::Protocol("seq exceeds i64".into()))
}

pub(crate) fn placeholder_digest() -> Digest {
    digest_of(b"")
}
