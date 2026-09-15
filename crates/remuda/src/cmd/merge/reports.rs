//! Persisted verification reports for `--onto` / `--land` / `--queue`.
//!
//! Reports live under the repository's common git directory, never in a
//! worktree checkout: `<git-common-dir>/remuda/merge-reports/<branch-slug>/<base>.json`.
//! A `<base>.preparing.json` sidecar appears as soon as the merge commit has
//! been constructed but before the gate runs, so a queue driver can start
//! speculatively verifying the next branch onto that commit.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::MergeReport;

pub(super) fn common_dir(repo: &Path) -> Result<PathBuf> {
    let raw = super::git(repo, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}

/// Filesystem-safe slug: `refs/heads/wt/a/b` -> `wt-a-b`.
pub(super) fn slug(reference: &str) -> String {
    let name = reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .trim_matches('/');
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn report_dir(repo: &Path, reference: &str) -> Result<PathBuf> {
    Ok(common_dir(repo)?
        .join("remuda/merge-reports")
        .join(slug(reference)))
}

pub(super) fn report_path(repo: &Path, reference: &str, base: &str) -> Result<PathBuf> {
    Ok(report_dir(repo, reference)?.join(format!("{base}.json")))
}

/// Ref pinning the constructed merge commit so it stays reachable for
/// `--land` after the temporary worktree is removed.
pub(super) fn verified_ref(reference: &str, base: &str) -> String {
    format!("refs/remuda/merge/{}/{base}", slug(reference))
}

fn preparing_path(repo: &Path, reference: &str, base: &str) -> Result<PathBuf> {
    Ok(report_dir(repo, reference)?.join(format!("{base}.preparing.json")))
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, contents).context("write report file")?;
    fs::rename(&temporary, path).context("publish report file")
}

/// Record the constructed merge commit before the gate runs, so a queue lane
/// can start speculatively verifying the next branch onto it.
pub(super) fn write_preparing(
    repo: &Path,
    reference: &str,
    base: &str,
    merged: &str,
    head: &str,
) -> Result<()> {
    let path = preparing_path(repo, reference, base)?;
    let contents = serde_json::json!({
        "branch": reference.trim_start_matches("refs/heads/"),
        "base": base,
        "head": head,
        "merged": merged,
    });
    write_atomic(&path, &contents.to_string())
}

/// A merge commit that is constructed but not yet gate-verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Preparing {
    pub merged: String,
    pub head: String,
}

/// Read the preparing sidecar if it has been published yet.
pub(super) fn preparing(repo: &Path, reference: &str, base: &str) -> Result<Option<Preparing>> {
    let path = preparing_path(repo, reference, base)?;
    if !path.is_file() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path)?).context("parse preparing report")?;
    Ok(Some(Preparing {
        merged: value
            .get("merged")
            .and_then(|v| v.as_str())
            .context("preparing report missing merged")?
            .into(),
        head: value
            .get("head")
            .and_then(|v| v.as_str())
            .context("preparing report missing head")?
            .into(),
    }))
}

/// Persist the final report and remove the preparing sidecar.
pub(super) fn save(repo: &Path, reference: &str, report: &MergeReport) -> Result<()> {
    let base = report
        .base
        .as_deref()
        .context("verification report has no base")?;
    let path = report_path(repo, reference, base)?;
    write_atomic(&path, &serde_json::to_string(report)?)?;
    let sidecar = preparing_path(repo, reference, base)?;
    if sidecar.exists() {
        let _ = fs::remove_file(sidecar);
    }
    Ok(())
}

/// Load the verification report for exactly this (branch, base) pair.
pub(super) fn load(repo: &Path, reference: &str, base: &str) -> Result<MergeReport> {
    let path = report_path(repo, reference, base)?;
    let contents = fs::read_to_string(&path).with_context(|| {
        format!(
            "no verification report for {} on {base}; run --gate --onto first",
            reference.trim_start_matches("refs/heads/")
        )
    })?;
    serde_json::from_str(&contents).context("parse verification report")
}

/// Remove preparing sidecars a killed lane left behind (a normal finish
/// removes its own sidecar in [`save`]).
pub(super) fn cleanup_preparing(repo: &Path, reference: &str) -> Result<()> {
    let dir = report_dir(repo, reference)?;
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".preparing.json"))
        {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Delete every merge pin left for a branch after the queue settles.
/// Landed merges are reachable from main; dead speculative merges must not
/// stay pinned in the object store.
pub(super) fn cleanup_pins(repo: &Path, reference: &str) -> Result<()> {
    let prefix = format!("refs/remuda/merge/{}/", slug(reference));
    let listing = super::git(repo, &["for-each-ref", "--format=%(refname)", &prefix])?;
    for reference in listing.lines().filter(|line| !line.is_empty()) {
        let _ = super::git(repo, &["update-ref", "-d", reference]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_keep_branch_characters_and_replace_separators() {
        assert_eq!(slug("refs/heads/wt/a/b"), "wt-a-b");
        assert_eq!(slug("topic"), "topic");
        assert_eq!(slug("refs/heads/feature.1_x"), "feature.1_x");
    }

    #[test]
    fn preparing_sidecars_live_next_to_the_common_git_dir() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/a/work";
        assert_eq!(preparing(repo, reference, "base1").unwrap(), None);
        write_preparing(repo, reference, "base1", "merge1", "head1").unwrap();
        assert_eq!(
            preparing(repo, reference, "base1").unwrap(),
            Some(Preparing {
                merged: "merge1".into(),
                head: "head1".into(),
            })
        );
        let common = common_dir(repo).unwrap();
        assert!(
            common
                .join("remuda/merge-reports/wt-a-work/base1.preparing.json")
                .is_file()
        );
        assert!(
            report_path(repo, reference, "base1")
                .unwrap()
                .ends_with("remuda/merge-reports/wt-a-work/base1.json")
        );
        assert!(load(repo, reference, "base1").is_err());
    }

    #[test]
    fn passing_reports_round_trip_next_to_the_common_git_dir() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/a/work";
        let mut report: MergeReport = serde_json::from_value(serde_json::json!({
            "exitCode": 0,
            "status": "verified",
            "branch": "wt/a/work",
            "dryRun": false,
            "affected": true,
            "base": "0123456789abcdef",
            "head": "fedcba9876543210",
            "tree": "deadbeef",
            "merged": "cafef00d",
            "gateOverride": false,
            "web": false,
            "webE2e": false,
            "mainUpdated": false,
            "pushed": false,
            "conflicts": [],
            "steps": []
        }))
        .unwrap();
        save(repo, reference, &report).unwrap();
        let loaded = load(repo, reference, "0123456789abcdef").unwrap();
        assert_eq!(loaded.base.as_deref(), Some("0123456789abcdef"));
        assert_eq!(loaded.head.as_deref(), Some("fedcba9876543210"));
        assert_eq!(loaded.tree.as_deref(), Some("deadbeef"));
        assert_eq!(loaded.merged.as_deref(), Some("cafef00d"));
        assert!(
            common_dir(repo)
                .unwrap()
                .join("remuda/merge-reports/wt-a-work/0123456789abcdef.json")
                .is_file()
        );
        assert!(load(repo, reference, "other").is_err());
        // Save removes a stale preparing sidecar.
        write_preparing(repo, reference, "0123456789abcdef", "cafef00d", "fedcba").unwrap();
        save(repo, reference, &report).unwrap();
        let common = common_dir(repo).unwrap();
        assert!(
            !common
                .join("remuda/merge-reports/wt-a-work/0123456789abcdef.preparing.json")
                .exists()
        );
        report.exit_code = 1;
        report.status = "gate_failed".into();
        save(repo, reference, &report).unwrap();
        assert_eq!(
            load(repo, reference, "0123456789abcdef").unwrap().exit_code,
            1
        );
    }
}
