//! R11 (r32): ONE hardened invocation for EVERY git command recovery runs.
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
//! commands with [`RecoveryGit::command`]. The empty hooks directory lives in
//! a repo-local scratch path (never `/tmp`) and is removed on drop.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// A process-lifetime guard holding the per-recovery hardening state. Create
/// ONE at recovery entry and build every recovery git command through it; the
/// scratch hooks directory is removed when it drops.
pub struct RecoveryGit {
    /// Empty directory handed to `core.hooksPath`; also anchors the
    /// nonexistent graft-file path.
    hooks_dir: PathBuf,
}

impl RecoveryGit {
    /// Create the empty hooks scratch directory under the repository working
    /// tree (a dot-directory the recovery owns and deletes — never `/tmp`, so
    /// per-process paths stay on the repo's filesystem and are cleaned up).
    pub fn enter(repo: &Path) -> std::io::Result<Self> {
        let hooks_dir = repo.join(".git-r32-recovery-no-hooks");
        std::fs::create_dir_all(&hooks_dir)?;
        Ok(Self { hooks_dir })
    }

    /// Build a hardened `git` command for the recovery subcommand in `args`
    /// (e.g. `["ls-remote", "--exit-code", "origin", ref]`). Hardening `-c`
    /// options are inserted before the subcommand; the fixed environment is
    /// applied regardless of the caller's environment.
    pub fn command(&self, repo: &Path, args: &[&str]) -> Command {
        let mut full: Vec<OsString> = Vec::with_capacity(8 + args.len());
        let push = |full: &mut Vec<OsString>, value: &str| full.push(OsString::from(value));
        // No automatic garbage collection: a fetch must never spawn gc, which
        // can detach beyond the recovery owner group and deadline.
        push(&mut full, "-c");
        push(&mut full, "gc.auto=0");
        // No scheduled/background maintenance either.
        push(&mut full, "-c");
        push(&mut full, "maintenance.auto=false");
        // Do not descend into submodules regardless of fetch.recurseSubmodules.
        push(&mut full, "-c");
        push(&mut full, "fetch.recurseSubmodules=false");
        // Run NO repository hooks (empty, recovery-owned directory).
        push(&mut full, "-c");
        push(
            &mut full,
            &format!("core.hooksPath={}", self.hooks_dir.display()),
        );
        for arg in args {
            push(&mut full, arg);
        }

        let mut command = Command::new("git");
        command.current_dir(repo).args(full);
        // Ignore refs/replace so a local graft cannot fabricate ancestry.
        command.env("GIT_NO_REPLACE_OBJECTS", "1");
        // Point legacy graft lookup at a nonexistent path so .git/info/grafts
        // (and any GIT_GRAFT_FILE already in the environment) is ignored.
        command.env("GIT_GRAFT_FILE", self.hooks_dir.join("nonexistent-grafts"));
        // Never block waiting for interactive credentials.
        command.env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// The fixed environment this hardening applies (key, value), for tests
    /// asserting the complete hardening set.
    pub fn env_pairs(&self) -> [(&'static str, PathBuf); 3] {
        [
            ("GIT_NO_REPLACE_OBJECTS", PathBuf::from("1")),
            ("GIT_GRAFT_FILE", self.hooks_dir.join("nonexistent-grafts")),
            ("GIT_TERMINAL_PROMPT", PathBuf::from("0")),
        ]
    }

    /// The `-c key=value` hardening entries this hardening applies, for tests.
    pub fn config_pairs(&self) -> [(&'static str, String); 4] {
        [
            ("gc.auto", "0".to_owned()),
            ("maintenance.auto", "false".to_owned()),
            ("fetch.recurseSubmodules", "false".to_owned()),
            ("core.hooksPath", self.hooks_dir.display().to_string()),
        ]
    }
}

impl Drop for RecoveryGit {
    fn drop(&mut self) {
        // Best-effort: the scratch directory is empty and owned by recovery.
        let _ = std::fs::remove_dir_all(&self.hooks_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every hardening entry — env, leading `-c` options, and the fetch-only
    /// flags — is present on a command built for any recovery subcommand. This
    /// is the single structural guarantee that no recovery invocation can
    /// bypass the R11 set.
    #[test]
    fn hardening_is_applied_to_every_recovery_command() {
        let tmp = tempfile_like_scratch();
        let h = RecoveryGit::enter(&tmp).expect("enter");
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
            for (key, value) in h.env_pairs() {
                assert!(
                    env.iter()
                        .any(|(k, v)| k == key && v == &value.to_string_lossy()),
                    "{args:?} missing env {key}={value:?}; got {env:?}"
                );
            }
            let argv: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            // The four hardening -c options precede the subcommand.
            let sub_index = argv
                .iter()
                .position(|a| !a.starts_with('-') && !a.contains('=') && !matches!(a.as_str(), "c"))
                .unwrap_or(argv.len());
            let prefix: Vec<&str> = argv[..sub_index].iter().map(String::as_str).collect();
            for (key, value) in h.config_pairs() {
                let expected = format!("{key}={value}");
                assert!(
                    prefix.windows(2).any(|w| w[0] == "-c" && w[1] == expected),
                    "{args:?} missing -c {expected}; prefix {prefix:?}"
                );
            }
            // Fetch carries the full fetch flag set.
            if args.first() == Some(&"fetch") {
                for flag in RECOVERY_FETCH_FLAGS {
                    assert!(argv.iter().any(|a| a == flag), "fetch missing {flag}");
                }
            }
        }
        drop(h);
        assert!(
            !tmp.join(".git-r32-recovery-no-hooks").exists(),
            "the hooks scratch dir is removed on drop"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A throwaway scratch "repository" directory inside the crate (never
    /// `/tmp`); removed when the guard drops.
    fn tempfile_like_scratch() -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".r32-hardening-scratch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
