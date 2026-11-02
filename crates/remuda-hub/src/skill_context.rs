//! ma-skill (D-060 item 5, main-agent.md §3.4.1): the launch facts the Hub
//! stamps as `skillContext` on a forwarded `instance.create` /
//! `instance.resume`, from which the Node's materializer installs the
//! embedded `remuda` skill and renders the seat's capability section.
//!
//! Like `memoryContext` the field is Hub-generated: every value is read from
//! the instance, host and project rows the Hub already enforces from, a
//! body-supplied copy is stripped before forwarding, and it never travels in
//! `InstanceSpec.args`. It is stamped only for agent kinds, and only for a
//! Node that advertises `capabilities.skillContext`; an older Node would drop
//! the unknown field silently. It grants nothing: each verb the section names
//! is still checked by its own handler on every call.

use crate::store::{HostRecord, InstanceRecord};
use remuda_protocol::hubnode::{SKILL_CONTEXT_CAPABILITY, SkillContext};
use serde_json::Value;

/// Whether the Hub delivers inbox facts to a seat. False until the inbox
/// lands (`cb-inbox-facts`, `cb-inbox-deliver`, main-agent.md §15.1).
const INBOX: bool = false;

/// Kinds whose launches read a skill or a first-input block as an agent.
fn is_agent_kind(kind: &str) -> bool {
    matches!(kind, "claude" | "codex" | "grok" | "agy")
}

/// Whether `host` says it carries `skillContext` to its materializer.
pub(crate) fn advertises(host: &HostRecord) -> bool {
    host.capabilities.get(SKILL_CONTEXT_CAPABILITY) == Some(&Value::Bool(true))
}

/// Whether `host` advertises herdr: a non-null `herdr` record, the same test
/// the dispatch carrier choice applies (`workers::select_carrier`).
fn advertises_herdr(host: &HostRecord) -> bool {
    host.herdr.as_ref().is_some_and(|value| !value.is_null())
}

/// The facts for one forwarded create or resume of `instance` on `host`, or
/// `None` when nothing is stamped (not an agent kind, or a Node that does not
/// advertise the capability). `delegated_decisions` is the D-051 switch as
/// the answer gate reads it for the instance's project.
pub(crate) fn facts(
    instance: &InstanceRecord,
    host: &HostRecord,
    delegated_decisions: bool,
) -> Option<SkillContext> {
    if !is_agent_kind(&instance.kind) || !advertises(host) {
        return None;
    }
    Some(SkillContext {
        grants: instance.grants.clone(),
        herdr: advertises_herdr(host),
        delegated_decisions,
        inbox: INBOX,
    })
}
