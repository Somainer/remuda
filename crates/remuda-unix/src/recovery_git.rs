//! R11 (r32/r33): ONE hardened invocation for EVERY git command recovery runs.
//!
//! Recovery answers an authoritative question — "did the granted merge actually
//! reach the remote?" — over a repository the local operator may partially
//! control. Several local git features can rewrite what recovery reads or
//! spawn work outside the R6 owner group:
//! * replacement refs (`git replace --graft`) and the legacy
//!   `.git/info/grafts` file rewrite parent/ancestry reads, so a divergent
//!   remote tip can falsely report the granted merge as an ancestor;
//! * interactive credential helpers can block;
//! * fetch-triggered `gc`/`maintenance` can detach into the background,
//!   escaping the owner and the absolute deadline while mutating storage;
//! * recursive submodule fetching runs additional fetches (with THEIR OWN
//!   configured mappings) and fails recovery when a submodule remote is down;
//! * repository hooks are arbitrary local code.
//!
//! [`RecoveryGit`] applies one fixed, documented hardening set to every
//! invocation; callers cannot bypass it because recovery builds all of its git
//! commands with [`RecoveryGit::command`].
//!
//! r33 (coordinator clarification): the guard creates, adopts or deletes NO
//! filesystem path. The empty device `/dev/null` neutralizes both file-based
//! inputs without a scratch directory: `GIT_GRAFT_FILE=/dev/null` is an always
//! empty grafts file, and `-c core.hooksPath=/dev/null` makes every hook lookup
//! miss (the path is never a directory containing an executable). Two
//! recoveries therefore share no state, and pre-existing repository content
//! cannot be adopted or removed.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(test, feature = "test-faults"))]
use std::sync::{Arc, Mutex, OnceLock};

/// Fetch subcommand flags appended after `fetch`. Kept here so Node and CLI
/// recovery cannot drift. See [`RecoveryGit::command`] for the env/config layer.
pub const RECOVERY_FETCH_FLAGS: &[&str] = &[
    "--no-tags",
    // Never follow configured tagOpt=--tags (a clobbered local tag must not fail recovery).
    "--no-prune",
    // Never delete stale remote-tracking refs (fetch.prune / origin.prune).
    "--refmap=",
    // Ignore every configured remote.origin.fetch destination mapping.
    "--no-recurse-submodules",
    // Never recurse into populated submodules (their mappings/remotes do not apply).
    "--no-auto-gc",
    // Older-compatible switch disabling fetch-triggered auto-gc on the fetch itself.
];

/// Hardening `-c key=value` entries applied before every recovery subcommand.
/// `core.hooksPath` points at the empty DEVICE (`/dev/null`), not a directory
/// recovery creates, so no hook lookup can resolve and recovery never writes
/// or deletes a hooks path.
const HARDENING_CONFIG: &[(&str, &str)] = &[
    // No automatic garbage collection: a fetch must never spawn gc, which can
    // detach beyond the recovery owner group and deadline.
    ("gc.auto", "0"),
    // No scheduled/background maintenance either.
    ("maintenance.auto", "false"),
    // Do not descend into submodules regardless of fetch.recurseSubmodules.
    ("fetch.recurseSubmodules", "false"),
    // Run NO repository hooks: /dev/null can never resolve to a hook program.
    ("core.hooksPath", "/dev/null"),
];

/// Hardening environment variables applied to every recovery git command.
const HARDENING_ENV: &[(&str, &str)] = &[
    // Ignore refs/replace so a local graft cannot fabricate ancestry.
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    // The empty device is an always-empty legacy grafts file, so a planted
    // .git/info/grafts (or a GIT_GRAFT_FILE already in the environment) can
    // never rewrite parent/ancestry reads.
    ("GIT_GRAFT_FILE", "/dev/null"),
    // Never block waiting for interactive credentials.
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// The recovery hardening guard. It holds NO filesystem state and performs NO
/// filesystem work — create one per recovery (overlapping recoveries are
/// fully independent) and drop it freely; dropping deletes nothing.
#[derive(Default)]
pub struct RecoveryGit {
    /// When set, every git command gets `GIT_TRACE=<path>`; recovery itself
    /// leaves this `None`. Used by tests to prove no gc/maintenance spawns.
    trace_file: Option<PathBuf>,
}

impl RecoveryGit {
    /// Create a recovery hardening guard. Creates nothing on disk.
    pub fn new() -> Self {
        Self::default()
    }

    /// Variant of [`Self::new`] that also sends GIT_TRACE for every git command
    /// to `trace_file`; used to prove recovery never spawns gc/maintenance.
    /// Recovery's own entry point uses plain [`Self::new`].
    pub fn traced(trace_file: &Path) -> Self {
        Self {
            trace_file: Some(trace_file.to_path_buf()),
        }
    }

    /// Build a hardened `git` command for the recovery subcommand in `args`
    /// (e.g. `["ls-remote", "--exit-code", "origin", ref]`). Hardening `-c`
    /// options are inserted before the subcommand; the fixed environment is
    /// applied regardless of the caller's environment.
    pub fn command(&self, repo: &Path, args: &[&str]) -> Command {
        let mut full: Vec<OsString> = Vec::with_capacity(2 * HARDENING_CONFIG.len() + args.len());
        for (key, value) in HARDENING_CONFIG {
            full.push(OsString::from("-c"));
            full.push(OsString::from(format!("{key}={value}")));
        }
        for arg in args {
            full.push(OsString::from(arg));
        }

        let mut command = Command::new("git");
        command.current_dir(repo).args(full);
        for (key, value) in HARDENING_ENV {
            command.env(key, value);
        }
        // Optionally trace every spawned child so gc/maintenance detachment
        // is observable (tests only; recovery leaves this unset).
        if let Some(trace_file) = &self.trace_file {
            command.env("GIT_TRACE", trace_file);
        }
        command
    }

    /// The fixed environment this hardening applies, for tests.
    pub fn env_pairs() -> [(&'static str, &'static str); 3] {
        HARDENING_ENV
            .try_into()
            .expect("HARDENING_ENV has exactly three entries")
    }

    /// The `-c key=value` hardening entries this hardening applies, for tests.
    pub fn config_pairs() -> [(&'static str, &'static str); 4] {
        HARDENING_CONFIG
            .try_into()
            .expect("HARDENING_CONFIG has exactly four entries")
    }
}

/// One git invocation observed at the actual owner-spawn boundary
/// ([`run_owned_process`]), so tests can prove the REAL recovery paths issue
/// every command through [`RecoveryGit`] — a caller bypassing the builder
/// would show missing argv/env entries here.
#[cfg(any(test, feature = "test-faults"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedGit {
    /// Full argv (argv[0] omitted), including the hardening `-c` prefix.
    pub args: Vec<String>,
    /// The explicitly-set environment on the spawned command (the hardening
    /// env is set explicitly, so it is observable here).
    pub env: Vec<(String, String)>,
}

/// The optional process-wide recorder installed by tests for the duration of a
/// recovery run. `None` (the production default) records nothing and adds only
/// a cheap `Option` check at spawn time.
#[cfg(any(test, feature = "test-faults"))]
type SpawnLog = Arc<Mutex<Vec<RecordedGit>>>;

#[cfg(any(test, feature = "test-faults"))]
static GIT_SPAWN_RECORDER: OnceLock<Mutex<Option<SpawnLog>>> = OnceLock::new();

#[cfg(any(test, feature = "test-faults"))]
fn recorder() -> &'static Mutex<Option<SpawnLog>> {
    GIT_SPAWN_RECORDER.get_or_init(|| Mutex::new(None))
}

/// Install (or clear with `None`) the spawn recorder used by recovery tests.
#[cfg(any(test, feature = "test-faults"))]
pub fn set_git_spawn_recorder(log: Option<Arc<Mutex<Vec<RecordedGit>>>>) {
    *recorder().lock().expect("recorder lock") = log;
}

/// Record `command` at spawn time when a test recorder is installed. Called by
/// [`crate::run_owned_process`] before the child is spawned.
#[cfg(any(test, feature = "test-faults"))]
pub(crate) fn record_spawn(command: &Command) {
    let guard = recorder().lock().expect("recorder lock");
    if let Some(log) = guard.as_ref() {
        let args = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap_or_default().to_string_lossy().into_owned(),
                )
            })
            .collect();
        log.lock()
            .expect("record log lock")
            .push(RecordedGit { args, env });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every hardening entry — env, leading `-c` options, and the fetch-only
    /// flags — is present on a command built for any recovery subcommand. This
    /// is the single structural guarantee that no recovery invocation can
    /// bypass the R11 set. It creates NO filesystem path.
    #[test]
    fn hardening_is_applied_to_every_recovery_command() {
        let tmp = tempfile_like_scratch();
        let h = RecoveryGit::new();
        for args in [
            &["ls-remote", "--exit-code", "origin", "refs/heads/main"][..],
            &[
                "fetch",
                "--no-tags",
                "--no-prune",
                "--refmap=",
                "--no-recurse-submodules",
                "--no-auto-gc",
                "origin",
                "<sha>",
            ][..],
            &["cat-file", "-e", "<sha>^{commit}"][..],
            &["merge-base", "--is-ancestor", "<m>", "<r>"][..],
            &["rev-parse", "--is-shallow-repository"][..],
        ] {
            let command = h.command(&tmp, args);
            let env: Vec<_> = command
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.unwrap_or_default().to_string_lossy().into_owned(),
                    )
                })
                .collect();
            for (key, value) in RecoveryGit::env_pairs() {
                assert!(
                    env.iter().any(|(k, v)| k == key && v == value),
                    "{args:?} missing env {key}={value}; got {env:?}"
                );
            }
            let argv: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            // argv is EXACTLY four "-c key=value" pairs followed by caller args
            // — nothing can be dropped, reordered, or bypassed.
            assert_eq!(
                argv.len(),
                2 * RecoveryGit::config_pairs().len() + args.len(),
                "{args:?} argv has an unexpected shape: {argv:?}"
            );
            assert_eq!(
                argv[2 * RecoveryGit::config_pairs().len()],
                args[0],
                "the subcommand must follow the four -c pairs"
            );
            for (index, (key, value)) in RecoveryGit::config_pairs().iter().enumerate() {
                assert_eq!(
                    argv[2 * index],
                    "-c",
                    "hardening pair {index} starts with -c"
                );
                assert_eq!(
                    &argv[2 * index + 1],
                    &format!("{key}={value}"),
                    "hardening pair {index} has the wrong value"
                );
            }
            for (offset, arg) in args.iter().enumerate() {
                assert_eq!(
                    argv[2 * RecoveryGit::config_pairs().len() + offset],
                    *arg,
                    "caller args must pass through unchanged"
                );
            }
            if args.first() == Some(&"fetch") {
                for flag in RECOVERY_FETCH_FLAGS {
                    assert!(argv.iter().any(|a| a == flag), "fetch missing {flag}");
                }
            }
        }
        // r33: the guard creates/uses/deletes no scratch directory; the old
        // r32 hardening path must never appear, and dropping the guard removes
        // nothing.
        drop(h);
        assert!(
            !tmp.join(".git-r32-recovery-no-hooks").exists(),
            "no scratch directory is created anywhere"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A throwaway scratch "repository" directory inside the crate (never
    /// `/tmp`); used only to supply a current_dir to the command builder.
    fn tempfile_like_scratch() -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".r33-hardening-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
