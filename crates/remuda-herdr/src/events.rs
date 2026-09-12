//! `events.subscribe` params and event envelopes.
//!
//! Subscription `type` uses dots (`pane.agent_status_changed`). The `event`
//! field on the wire is inconsistent (dots or underscores); [`Event::name`] is
//! always normalized to underscores.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::AgentStatus;

/// One subscription record in `events.subscribe.params.subscriptions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// Subscription type (dotted, e.g. `pane.agent_status_changed`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Required for `pane.agent_status_changed` (cannot subscribe to all panes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Optional status filter for agent-status subscriptions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<AgentStatus>,
}

impl Subscription {
    /// Subscribe to workspace/tab/pane lifecycle (`pane.created`, `pane.exited`, …).
    #[must_use]
    pub fn global(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            pane_id: None,
            agent_status: None,
        }
    }

    /// `pane.created`.
    #[must_use]
    pub fn pane_created() -> Self {
        Self::global("pane.created")
    }

    /// `pane.exited`.
    #[must_use]
    pub fn pane_exited() -> Self {
        Self::global("pane.exited")
    }

    /// `pane.updated` (geometry / title). Output bytes are not included.
    #[must_use]
    pub fn pane_updated() -> Self {
        Self::global("pane.updated")
    }

    /// `pane.agent_status_changed` — `pane_id` is mandatory on Herdr 0.9.0.
    #[must_use]
    pub fn pane_agent_status_changed(pane_id: impl Into<String>) -> Self {
        Self {
            kind: "pane.agent_status_changed".into(),
            pane_id: Some(pane_id.into()),
            agent_status: None,
        }
    }
}

/// `events.subscribe` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsSubscribeParams {
    /// Subscriptions. Frozen at request time; new panes need a new connection.
    pub subscriptions: Vec<Subscription>,
}

/// Normalized event name (underscores).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Workspace created.
    WorkspaceCreated,
    /// Workspace updated.
    WorkspaceUpdated,
    /// Workspace closed.
    WorkspaceClosed,
    /// Tab created.
    TabCreated,
    /// Tab closed.
    TabClosed,
    /// Pane created.
    PaneCreated,
    /// Pane closed.
    PaneClosed,
    /// Pane updated.
    PaneUpdated,
    /// Terminal produced output (dirty flag only).
    PaneOutputChanged,
    /// Foreground process exited.
    PaneExited,
    /// Agent detected on a pane.
    PaneAgentDetected,
    /// Agent status transition.
    PaneAgentStatusChanged,
    /// Layout changed.
    LayoutUpdated,
    /// Anything else (tolerate schema drift).
    Unknown,
}

impl EventKind {
    /// Parse dotted or underscored names.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match normalize_event_name(name).as_str() {
            "workspace_created" => Self::WorkspaceCreated,
            "workspace_updated" => Self::WorkspaceUpdated,
            "workspace_closed" => Self::WorkspaceClosed,
            "tab_created" => Self::TabCreated,
            "tab_closed" => Self::TabClosed,
            "pane_created" => Self::PaneCreated,
            "pane_closed" => Self::PaneClosed,
            "pane_updated" => Self::PaneUpdated,
            "pane_output_changed" => Self::PaneOutputChanged,
            "pane_exited" => Self::PaneExited,
            "pane_agent_detected" => Self::PaneAgentDetected,
            "pane_agent_status_changed" => Self::PaneAgentStatusChanged,
            "layout_updated" => Self::LayoutUpdated,
            _ => Self::Unknown,
        }
    }
}

/// One event line from a subscribe connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Normalized underscore name (`pane_agent_status_changed`).
    pub name: String,
    /// Wire `event` field as received.
    pub raw_name: String,
    /// Parsed kind.
    pub kind: EventKind,
    /// Raw `data` object.
    pub data: Value,
}

impl Event {
    /// Build from a decoded envelope.
    #[must_use]
    pub fn from_wire(raw_name: String, data: Value) -> Self {
        let name = normalize_event_name(&raw_name);
        let kind = EventKind::parse(&name);
        Self {
            name,
            raw_name,
            kind,
            data,
        }
    }

    /// `pane_id` if the payload has one.
    #[must_use]
    pub fn pane_id(&self) -> Option<&str> {
        self.data
            .get("pane_id")
            .and_then(Value::as_str)
            .or_else(|| {
                self.data
                    .get("pane")
                    .and_then(|pane| pane.get("pane_id"))
                    .and_then(Value::as_str)
            })
    }

    /// `agent_status` if present.
    #[must_use]
    pub fn agent_status(&self) -> Option<AgentStatus> {
        let value = self.data.get("agent_status")?;
        serde_json::from_value(value.clone()).ok()
    }

    /// `workspace_id` if present.
    #[must_use]
    pub fn workspace_id(&self) -> Option<&str> {
        self.data.get("workspace_id").and_then(Value::as_str)
    }
}

/// Dots → underscores so clients can match one spelling.
#[must_use]
pub fn normalize_event_name(name: &str) -> String {
    name.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotted_and_underscored_event_names() {
        assert_eq!(
            EventKind::parse("pane.agent_status_changed"),
            EventKind::PaneAgentStatusChanged
        );
        assert_eq!(
            EventKind::parse("pane_agent_status_changed"),
            EventKind::PaneAgentStatusChanged
        );
        assert_eq!(EventKind::parse("pane_created"), EventKind::PaneCreated);
        assert_eq!(EventKind::parse("pane.created"), EventKind::PaneCreated);
    }
}
