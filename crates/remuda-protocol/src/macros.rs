//! Shared declarations keep the wire names and specification references explicit.

macro_rules! wire_enum {
    ($name:ident, $section:literal, { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[doc = concat!(stringify!($name), " wire values; `protocol.md` §", $section, ".")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        pub enum $name {
            $(#[doc = concat!("Wire value `", $wire, "`.")]
              #[serde(rename = $wire)] $variant),+
        }
    };
}
