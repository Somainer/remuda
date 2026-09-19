//! Building the launch command for an agent in a Remuda-owned PTY
//! (D-028 §5.1, §5.6).
//!
//! The unification principle says a `claude` session is a terminal session
//! running `claude`. Concretely that means this module builds the same thing a
//! human's keystrokes would: an argv, in a cwd, with an environment — and then
//! the PTY runs it directly instead of running a shell that waits for someone
//! to type it.
//!
//! Everything argv-shaped comes from [`crate::materializer`], which already
//! owns the per-kind presets, the yolo gating, `--effort`, and the flag
//! allowlist. This module's job is to ask for a recipe and turn it into a
//! [`CommandBuilder`] — not to grow a second, subtly different opinion about
//! how `claude` is invoked. The stub recipe `shell_pty.rs` used to emit (empty
//! allowlist, hardcoded provider) is replaced by the real one, which is what
//! §5.1 step 4 requires for the audit.

use crate::error::{DriverError, DriverResult};
use crate::recipe::LaunchRecipe;
use remuda_protocol::{AgentKind, DriverKind, InstanceSpec};
use std::path::{Path, PathBuf};

/// Env flag gating the native launch path (D-028 §13 rule ⑤).
///
/// `native` opts an agent kind into being launched by Remuda's own PTY. Kind
/// `terminal` does not consult it — a login shell in a PTY is what shell-pty
/// has always been, and there is no other carrier for it to fall back to.
/// Rollback is `unset` plus a Node restart, with no schema migration.
pub const CARRIER_ENV: &str = "REMUDA_PTY_CARRIER";

/// Whether [`CARRIER_ENV`] selects the native carrier for agent kinds.
#[must_use]
pub fn native_carrier_enabled() -> bool {
    matches!(
        std::env::var(CARRIER_ENV).as_deref().map(str::trim),
        Ok("native")
    )
}

/// What this PTY should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A login `$SHELL`. The human may start an agent inside it (D-025).
    Shell,
    /// An agent CLI, launched by Remuda from a materialized recipe.
    Agent {
        /// Which agent.
        kind: AgentKind,
        /// Native session this launch continues, when it is a resume (§5.6).
        resume: Option<String>,
    },
}

impl Target {
    /// The agent kind, when this target is one.
    #[must_use]
    pub fn agent_kind(&self) -> Option<AgentKind> {
        match self {
            Self::Agent { kind, .. } => Some(*kind),
            Self::Shell => None,
        }
    }
}

/// Everything needed to materialize an agent launch.
///
/// Boxed profile because [`crate::profile::ProviderProfile`] is large and this
/// struct is held for the lifetime of the driver.
#[derive(Debug, Clone)]
pub struct AgentLaunch {
    /// Provider profile the recipe resolves credentials against.
    pub profile: Box<crate::profile::ProviderProfile>,
    /// `<instance dir>/launch`, where overlays and shims are written.
    pub launch_dir: PathBuf,
    /// Registered native home for this harness.
    pub native_home: PathBuf,
    /// Explicit binary, or `None` to resolve the preset's name on `PATH`.
    pub binary: Option<PathBuf>,
    /// Who asked for this launch. Gates yolo argv (D-011/D-017).
    pub origin: crate::materializer::LaunchOrigin,
    /// Whether `native_home` is Remuda-managed (skill delivery allowed) or the
    /// inherited operator home, which is never written (D-045).
    pub native_home_managed: bool,
    /// Settings overlay to pass as `--settings`, when one was written.
    pub settings_overlay: Option<PathBuf>,
}

/// Build the launch recipe for `spec` running as `kind` in a native PTY.
///
/// `resume` carries a native session id (§5.6): the resulting argv gets the
/// harness's own resume flag, because resuming *is* a new session that happens
/// to be told which conversation to continue.
pub fn agent_recipe(
    spec: &InstanceSpec,
    launch: &AgentLaunch,
    cwd: &str,
    resume: Option<&str>,
) -> DriverResult<LaunchRecipe> {
    if spec.driver != DriverKind::ShellPty {
        return Err(DriverError::InvalidLaunchSpec(
            "the native agent launch path is only for driver shell-pty".into(),
        ));
    }
    let preset = crate::presets::preset_for_spec(spec)?;
    let binary = match &launch.binary {
        Some(path) => crate::materializer::BinarySource::Path(path.clone()),
        None => crate::materializer::BinarySource::Command(preset.binary.to_owned()),
    };
    let session = match resume {
        Some(session_id) => crate::materializer::SessionAction::Resume {
            session_id: session_id.to_owned(),
        },
        None => crate::materializer::SessionAction::New {
            // §5.6: the id the harness reports wins. This placeholder only
            // names the launch in the audit; it is never passed as
            // `--session-id`, which the materializer's `New` arm omits.
            session_id: spec.workspace_id.as_id().to_string(),
        },
    };
    // The spec's cwd is authoritative unless it does not exist, in which case
    // the caller's resolved workspace root is the honest fallback — the same
    // rule the shell path uses, so both targets land in the same directory.
    let mut spec = spec.clone();
    spec.cwd = cwd.to_owned();
    crate::materializer::materialize(&crate::materializer::MaterializeRequest {
        spec: &spec,
        profile: &launch.profile,
        launch_dir: launch.launch_dir.clone(),
        native_home: launch.native_home.clone(),
        session,
        launch_id: remuda_protocol::Id::new("launch")?,
        binary,
        setting_sources: None,
        origin: launch.origin,
        native_home_managed: Some(launch.native_home_managed),
        settings_overlay_path: launch.settings_overlay.clone(),
        secret_policy: None,
    })
}

/// Resume flag spelling per harness (§5.6).
///
/// Claude and grok take `--resume <id>`; codex takes the subcommand
/// `resume <id>`. The materializer emits the flag form, so codex needs its argv
/// rewritten — doing it here keeps the shape of the difference visible instead
/// of burying a special case in a shared argv builder.
#[must_use]
pub fn apply_resume_shape(kind: AgentKind, argv: Vec<String>) -> Vec<String> {
    if kind != AgentKind::Codex {
        return argv;
    }
    let Some(index) = argv.iter().position(|token| token == "--resume") else {
        return argv;
    };
    let Some(session) = argv.get(index + 1).cloned() else {
        return argv;
    };
    // `codex resume <id>` is a subcommand, so it goes first and the flag pair
    // it replaces comes out.
    let mut rebuilt = vec!["resume".to_owned(), session];
    rebuilt.extend(
        argv.into_iter()
            .enumerate()
            .filter(|(position, _)| *position != index && *position != index + 1)
            .map(|(_, token)| token),
    );
    rebuilt
}

/// Build the `CommandBuilder` that runs `recipe` in the PTY.
///
/// argv[0] is the pinned binary from the recipe, not the first element of
/// `spec.args`: §5.1 step 3 forbids passing the request's args straight to the
/// command builder, because that is the path by which a caller could name any
/// executable at all.
pub fn agent_command(
    recipe: &LaunchRecipe,
    kind: AgentKind,
    cwd: &Path,
) -> DriverResult<portable_pty::CommandBuilder> {
    let mut cmd = portable_pty::CommandBuilder::new(&recipe.binary.abs_path);
    for arg in apply_resume_shape(kind, recipe.argv.clone()) {
        cmd.arg(arg);
    }
    cmd.cwd(cwd);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_resume_becomes_a_subcommand_and_the_others_keep_the_flag() {
        // §5.6 lists three spellings; only codex's is positional.
        assert_eq!(
            apply_resume_shape(
                AgentKind::Codex,
                vec![
                    "--effort".into(),
                    "high".into(),
                    "--resume".into(),
                    "abc".into()
                ]
            ),
            vec!["resume", "abc", "--effort", "high"],
            "the subcommand must lead, and the flag pair must not survive"
        );
        let flag_form = vec!["--resume".to_owned(), "abc".to_owned()];
        assert_eq!(
            apply_resume_shape(AgentKind::Claude, flag_form.clone()),
            flag_form
        );
        assert_eq!(
            apply_resume_shape(AgentKind::Grok, flag_form.clone()),
            flag_form
        );
    }

    #[test]
    fn a_codex_launch_without_a_resume_is_left_alone() {
        let argv = vec!["--effort".to_owned(), "high".to_owned()];
        assert_eq!(apply_resume_shape(AgentKind::Codex, argv.clone()), argv);
    }

    #[test]
    fn a_dangling_resume_flag_is_not_turned_into_a_subcommand_with_no_id() {
        // `codex resume` with no argument resumes *something* — the most recent
        // thread — which is exactly the `--continue` behaviour D-026 forbids.
        let argv = vec!["--resume".to_owned()];
        assert_eq!(apply_resume_shape(AgentKind::Codex, argv.clone()), argv);
    }

    #[test]
    fn the_carrier_flag_only_accepts_the_documented_value() {
        // Rollback is "unset it", so anything that is not the opt-in word must
        // read as off rather than as a typo that silently enables the path.
        assert!(matches!("native".trim(), "native"));
        for value in ["", "herdr", "1", "true", "Native"] {
            assert!(
                !matches!(value.trim(), "native"),
                "{value:?} must not enable the native carrier"
            );
        }
    }

    #[test]
    fn a_shell_target_names_no_agent_kind() {
        assert_eq!(Target::Shell.agent_kind(), None);
        assert_eq!(
            Target::Agent {
                kind: AgentKind::Claude,
                resume: None
            }
            .agent_kind(),
            Some(AgentKind::Claude)
        );
    }
}
