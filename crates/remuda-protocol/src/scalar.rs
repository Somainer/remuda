//! Validated scalar encodings and identity brands; `protocol.md` §1.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{fmt, str::FromStr};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::{Uuid, Variant};

/// Deserialize a required field whose explicit wire value may be null; §1.1.
pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// A malformed version, identifier, timestamp, or wire counter; `protocol.md` §1.1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WireValueError(pub String);

/// A lossless unsigned 64-bit counter encoded as a decimal JSON string; §1.1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct U64(pub u64);

impl Serialize for U64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for U64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(D::Error::custom("expected a canonical decimal U64 string"));
        }
        text.parse().map(Self).map_err(D::Error::custom)
    }
}

/// An opaque globally unique ID with a registered prefix and canonical UUIDv7; §1.2.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(String);

impl Id {
    /// Generate a UUIDv7 for a registered entity prefix; §1.2.
    pub fn new(prefix: &str) -> Result<Self, WireValueError> {
        format!("{prefix}_{}", Uuid::now_v7()).try_into()
    }

    /// Return the complete wire spelling, including its prefix; §1.2.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Id {
    type Error = WireValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let (prefix, suffix) = value
            .split_once('_')
            .ok_or_else(|| WireValueError("missing ID prefix".into()))?;
        if ![
            "hst", "wsp", "wkt", "ins", "run", "cmd", "int", "evt", "dev", "prn", "pvp", "cred",
            "obj", "sub", "tty", "launch", "epoch",
        ]
        .contains(&prefix)
        {
            return Err(WireValueError("unknown ID prefix".into()));
        }
        let uuid = Uuid::parse_str(suffix).map_err(|_| WireValueError("invalid UUID".into()))?;
        if uuid.get_version_num() != 7
            || uuid.get_variant() != Variant::RFC4122
            || uuid.to_string() != suffix
        {
            return Err(WireValueError(
                "ID requires a canonical lowercase UUIDv7".into(),
            ));
        }
        Ok(Self(value))
    }
}
impl From<Id> for String {
    fn from(value: Id) -> Self {
        value.0
    }
}
impl FromStr for Id {
    type Err = WireValueError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}
impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

macro_rules! branded_id {
    ($name:ident, $prefix:literal) => {
        #[doc = concat!(stringify!($name), " identity brand using `", $prefix, "_`; `protocol.md` §1.2.")]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Id);
        impl $name {
            /// Generate a new identity for this entity kind; §1.2.
            pub fn new() -> Self { Self(Id::new($prefix).expect("registered prefix")) }
            /// Return the validated untyped identity; §1.2.
            pub fn as_id(&self) -> &Id { &self.0 }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl TryFrom<String> for $name {
            type Error = WireValueError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                let id: Id = value.try_into()?;
                if !id.as_str().starts_with(concat!($prefix, "_")) {
                    return Err(WireValueError(concat!("expected ", $prefix, "_ ID").into()));
                }
                Ok(Self(id))
            }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0.into() } }
        impl FromStr for $name {
            type Err = WireValueError;
            fn from_str(value: &str) -> Result<Self, Self::Err> { value.to_owned().try_into() }
        }
    };
}
branded_id!(HostId, "hst");
branded_id!(WorkspaceId, "wsp");
branded_id!(WorktreeId, "wkt");
branded_id!(InstanceId, "ins");
branded_id!(RunId, "run");
branded_id!(CommandId, "cmd");
branded_id!(InteractionId, "int");
branded_id!(EventId, "evt");

/// UTC RFC3339 with exactly three fractional digits, serialized as text; §1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);
impl TryFrom<String> for Timestamp {
    type Error = WireValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 24 || !value.ends_with('Z') || value.as_bytes()[19] != b'.' {
            return Err(WireValueError(
                "expected UTC RFC3339 with milliseconds".into(),
            ));
        }
        OffsetDateTime::parse(&value, &Rfc3339)
            .map_err(|_| WireValueError("invalid timestamp".into()))?;
        Ok(Self(value))
    }
}
impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

/// A lowercase SHA-256 digest including the `sha256:` prefix; §1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);
impl TryFrom<String> for Digest {
    type Error = WireValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 71
            || !value.starts_with("sha256:")
            || !value.as_bytes()[7..]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return Err(WireValueError(
                "expected sha256 and 64 lowercase hex digits".into(),
            ));
        }
        Ok(Self(value))
    }
}
impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.0
    }
}

/// Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Knowledge<T> {
    /// A value supported by observations.
    Known {
        /// Recorded value.
        value: T,
    },
    /// A value for which the source is insufficient.
    Unknown {
        /// Stable reason code.
        reason: String,
        /// Supporting observation identities.
        evidence_event_ids: Vec<EventId>,
    },
    /// This value has no meaning for this variant.
    NotApplicable,
}

/// Schema major 1; future majors fail deserialization; `protocol.md` §§5.1, 9.2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct SchemaVersion;
impl TryFrom<u16> for SchemaVersion {
    type Error = WireValueError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self)
        } else {
            Err(WireValueError("unsupported schema major".into()))
        }
    }
}
impl From<SchemaVersion> for u16 {
    fn from(_: SchemaVersion) -> Self {
        1
    }
}

/// A boolean constrained to one protocol literal; `protocol.md` §§3.1, 7.2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoolLiteral<const VALUE: bool>;
impl<const VALUE: bool> Serialize for BoolLiteral<VALUE> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(VALUE)
    }
}
impl<'de, const VALUE: bool> Deserialize<'de> for BoolLiteral<VALUE> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? == VALUE {
            Ok(Self)
        } else {
            Err(D::Error::custom("invalid protocol boolean literal"))
        }
    }
}

/// A nonempty wire array, notably an event batch; `protocol.md` §7.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmpty<T>(Vec<T>);
impl<T> NonEmpty<T> {
    /// Reject empty arrays at construction; §7.3.
    pub fn new(items: Vec<T>) -> Result<Self, WireValueError> {
        if items.is_empty() {
            Err(WireValueError("empty event batch".into()))
        } else {
            Ok(Self(items))
        }
    }
    /// Access elements without permitting removal of the final entry; §7.3.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(Vec::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}
