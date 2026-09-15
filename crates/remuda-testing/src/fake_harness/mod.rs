//! Deterministic TUI stand-in for the `claude` / `codex` / `grok` harnesses.
//!
//! D-028 §12 item 7 / P7 fixture: one script-driven binary later phases launch
//! inside a real PTY. The emulation surface is documented in
//! `docs/design/testing-fake-harness.md`; see the submodules for the scenario
//! format ([`script`]), screen dialects ([`screen`]), hook execution
//! ([`hooks`]), artifact shapes ([`artifacts`]), PTY input chunk semantics
//! ([`input`]), and the deterministic clock ([`clock`]).

pub mod artifacts;
pub mod clock;
pub mod engine;
pub mod hooks;
pub mod input;
pub mod screen;
pub mod script;

pub use artifacts::{
    ArtifactKind, ArtifactPaths, ArtifactSet, ClaudeCounters, CodexCounters, GrokTurnIds,
    SessionMeta,
};
pub use clock::FakeClock;
pub use engine::{Options, RunError, run};
pub use hooks::{HookContext, HookEvent, HookKind, HookTable, decision_behavior, parse_decision};
pub use input::{Input, Parser as InputParser};
pub use screen::{
    ApprovalView, Dialect, DialectVersion, ScreenMode, View, WorkingPhase, enter, render_grid,
    repaint, teardown,
};
pub use script::{ApprovalMode, Scenario, ToolSpec, TurnSpec, UsageSpec};
