//! Notification tag and Web Push Topic (max 32 base64url chars).

use sha2::{Digest, Sha256};

/// Collapse key for a Hub notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushTag {
    /// `interaction:{id}` — pending approval or question.
    Interaction {
        /// Interaction identity.
        id: String,
    },
    /// `instance:{id}` — instance lifecycle / waiting-interaction.
    Instance {
        /// Instance identity.
        id: String,
    },
}

impl PushTag {
    /// Client-visible `tag` (and JSON `tag` field).
    pub fn as_str(&self) -> String {
        match self {
            Self::Interaction { id } => format!("interaction:{id}"),
            Self::Instance { id } => format!("instance:{id}"),
        }
    }

    /// RFC 8030 Topic header: at most 32 URL-safe base64 characters.
    pub fn topic(&self) -> String {
        let digest = Sha256::digest(self.as_str().as_bytes());
        crate::vapid::b64(&digest)[..32].to_owned()
    }
}

impl std::fmt::Display for PushTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}
