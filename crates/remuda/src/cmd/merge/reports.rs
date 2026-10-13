//! Persisted verification reports for `--onto` / `--land` / `--queue`.
//!
//! Two separate namespaces live under the repository's common git directory
//! (never in a worktree checkout), under
//! `<git-common-dir>/remuda/merge-reports/<branch-slug>/`:
//!
//! * latest-by-base: `<base>.json` — the most recent verification ONTO that
//!   base; a newer verification legitimately replaces it.
//! * immutable: `immutable/<merged>.json` — one exact (base, head, merged)
//!   tuple, published WITHOUT replacement. A pinned `--land --report <merged>`
//!   reads only this namespace, so a newer report (even on the same base) can
//!   never overwrite or be substituted for the tuple the gate captured.
//!
//! A `<base>.preparing.json` sidecar appears as soon as the merge commit has
//! been constructed but before the gate runs, so a queue driver can start
//! speculatively verifying the next branch onto that commit.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::MergeReport;

/// Process-unique nonce mixed into every staging filename, so two publications
/// in the same millisecond (and across lanes sharing this process) still get
/// distinct temp files.
static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// A UNIQUE staging path for one publication. The old
/// `<name>.json.tmp` constant was shared by every lane: a concurrent
/// publication's staging was overwritten mid-write and its own cleanup deleted
/// the other lane's bytes (r14 item 7). The name carries the pid, a
/// millisecond timestamp, and a per-process monotonic nonce.
fn unique_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "report".to_owned());
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{file_name}.{pid}.{micros}.{nonce}.tmp"))
}

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

/// Immutable, content-addressed report in its OWN namespace, keyed by the
/// constructed merge sha. This is distinct from the `<base>.json`
/// "latest verified onto this base" pointer — which a second verification
/// legitimately overwrites. The immutable file names one exact
/// (base, head, merged) tuple and is published WITHOUT replacement; a pinned
/// land reads THIS, so a newer same-base report cannot be substituted for an
/// older one.
fn immutable_report_path(repo: &Path, reference: &str, merged: &str) -> Result<PathBuf> {
    Ok(report_dir(repo, reference)?
        .join("immutable")
        .join(format!("{merged}.json")))
}

/// Publish a report as a NEW immutable identity, never replacing an existing
/// one. The contents are staged in a temp file and attached with an atomic
/// create-new (a hard link fails if the target already exists, and both files
/// are on the same git-dir filesystem). If an immutable record for this merge
/// sha already exists, it must name the same captured (base, head, merged)
/// tuple — a different record for the same identity is refused rather than
/// overwritten.
fn publish_immutable(path: &Path, contents: &str, captured: &MergeReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create report directory {}", parent.display()))?;
    }
    if path.is_file() {
        let existing = fs::read_to_string(path)
            .with_context(|| format!("read existing immutable report {}", path.display()))?;
        let existing: MergeReport =
            serde_json::from_str(&existing).context("parse existing immutable report")?;
        let same_tuple = existing.merged.as_deref() == captured.merged.as_deref()
            && existing.base.as_deref() == captured.base.as_deref()
            && existing.head.as_deref() == captured.head.as_deref();
        anyhow::ensure!(
            same_tuple,
            "immutable report {} already exists for a different (base, head, merged) tuple",
            captured.merged.as_deref().unwrap_or("<unknown merge>")
        );
        // Idempotent re-publish of the identical tuple: leave it untouched.
        return Ok(());
    }
    let temporary = unique_temp_path(path);
    fs::write(&temporary, contents).context("write report file")?;
    // Atomic create-new: fails if another lane published concurrently.
    match fs::hard_link(&temporary, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary);
            // Re-check under the now-present target: identical tuple is fine.
            let existing = fs::read_to_string(path)
                .with_context(|| format!("read existing immutable report {}", path.display()))?;
            let existing: MergeReport =
                serde_json::from_str(&existing).context("parse existing immutable report")?;
            anyhow::ensure!(
                existing.merged.as_deref() == captured.merged.as_deref()
                    && existing.base.as_deref() == captured.base.as_deref()
                    && existing.head.as_deref() == captured.head.as_deref(),
                "immutable report {} was concurrently published for a different tuple",
                captured.merged.as_deref().unwrap_or("<unknown merge>")
            );
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error).context("publish immutable report")
        }
    }
}

/// Load the immutable report for one captured merge sha.
pub(super) fn load_by_merged(repo: &Path, reference: &str, merged: &str) -> Result<MergeReport> {
    let path = immutable_report_path(repo, reference, merged)?;
    let contents = fs::read_to_string(&path).with_context(|| {
        format!(
            "no immutable verification report for merge {merged} on {}; re-run the gate",
            reference.trim_start_matches("refs/heads/")
        )
    })?;
    serde_json::from_str(&contents).context("parse immutable verification report")
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
    let temporary = unique_temp_path(path);
    let outcome = fs::write(&temporary, contents)
        .context("write report file")
        .and_then(|()| fs::rename(&temporary, path).context("publish report file"));
    if outcome.is_err() {
        // Remove only OUR staging file; never touch another lane's temp.
        let _ = fs::remove_file(&temporary);
    }
    outcome
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

/// Persist the final report and remove the preparing sidecar. Also publishes
/// the immutable `<merged>.json` copy a pinned `--land --report` reads.
pub(super) fn save(repo: &Path, reference: &str, report: &MergeReport) -> Result<()> {
    let base = report
        .base
        .as_deref()
        .context("verification report has no base")?;
    let path = report_path(repo, reference, base)?;
    let serialized = serde_json::to_string(report)?;
    write_atomic(&path, &serialized)?;
    if let Some(merged) = report.merged.as_deref() {
        let immutable = immutable_report_path(repo, reference, merged)?;
        if immutable != path {
            // Immutable identity: create-new, never replace a different tuple.
            publish_immutable(&immutable, &serialized, report)?;
        }
    }
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
    use crate::cmd::merge::{MergeArgs, new_report};

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

    #[test]
    fn a_pinned_report_survives_a_second_verification_on_the_same_base() {
        // r12 item 6: verify A then verify B for the SAME branch/base. The
        // `<base>.json` latest pointer is B, but the immutable report keyed by
        // A's merge sha must still resolve to A — exactly what a pinned
        // `--land --report <mergeA>` reads, so B cannot substitute A's land.
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/a/work";
        let base = "0123456789abcdef0123456789abcdef01234567";
        let make = |merged: &str, head: &str| MergeReport {
            merged: Some(merged.into()),
            head: Some(head.into()),
            ..serde_json::from_value(serde_json::json!({
                "exitCode": 0,
                "status": "verified",
                "branch": "wt/a/work",
                "dryRun": false,
                "affected": true,
                "base": base,
                "tree": "deadbeef",
                "gateOverride": false,
                "web": false,
                "webE2e": false,
                "mainUpdated": false,
                "pushed": false,
                "conflicts": [],
                "steps": []
            }))
            .unwrap()
        };
        let report_a = make(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "1111111111111111111111111111111111111111",
        );
        let report_b = make(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "2222222222222222222222222222222222222222",
        );
        save(repo, reference, &report_a).unwrap();
        save(repo, reference, &report_b).unwrap();

        // The latest-by-base pointer was overwritten by the second verify.
        let latest = load(repo, reference, base).unwrap();
        assert_eq!(
            latest.merged.as_deref(),
            Some(report_b.merged.as_deref().unwrap())
        );

        // The pinned immutable reports still select each exact tuple.
        let pinned_a =
            load_by_merged(repo, reference, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(
            pinned_a.merged.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            pinned_a.head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        let pinned_b =
            load_by_merged(repo, reference, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(
            pinned_b.merged.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );

        // The immutable records live in their own namespace, distinct from the
        // latest-by-base pointer.
        let common = common_dir(repo).unwrap();
        assert!(
            common
                .join("remuda/merge-reports/wt-a-work/immutable/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json")
                .is_file()
        );
        assert!(
            !common
                .join(
                    "remuda/merge-reports/wt-a-work/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json"
                )
                .is_file(),
            "immutable report must not share the latest-by-base namespace"
        );
    }

    #[test]
    fn an_immutable_report_is_never_replaced_by_a_different_tuple() {
        // r13 item 8: publishing a second record under the same merge sha with a
        // different (base, head) must NOT overwrite the first; it errors, and
        // the original tuple is still what load_by_merged returns. Re-publishing
        // the identical tuple is idempotent.
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/a/work";
        let merged = "dddddddddddddddddddddddddddddddddddddddd";
        let make = |base: &str, head: &str| MergeReport {
            merged: Some(merged.into()),
            head: Some(head.into()),
            ..serde_json::from_value(serde_json::json!({
                "exitCode": 0,
                "status": "verified",
                "branch": "wt/a/work",
                "dryRun": false,
                "affected": true,
                "base": base,
                "tree": "deadbeef",
                "gateOverride": false,
                "web": false,
                "webE2e": false,
                "mainUpdated": false,
                "pushed": false,
                "conflicts": [],
                "steps": []
            }))
            .unwrap()
        };
        let first = make(
            "1111111111111111111111111111111111111111",
            "aaaa11111111111111111111111111111111111",
        );
        save(repo, reference, &first).unwrap();

        // Identical tuple: idempotent, no error.
        save(repo, reference, &first).unwrap();

        // Different base/head under the SAME merged sha: refused.
        let conflicting = make(
            "2222222222222222222222222222222222222222",
            "bbbb22222222222222222222222222222222222",
        );
        let err = save(repo, reference, &conflicting).unwrap_err();
        assert!(
            err.to_string().contains("already exists for a different"),
            "{err}"
        );

        // The original immutable tuple is intact.
        let loaded = load_by_merged(repo, reference, merged).unwrap();
        assert_eq!(
            loaded.base.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(
            loaded.head.as_deref(),
            Some("aaaa11111111111111111111111111111111111")
        );
    }

    #[test]
    fn concurrent_publications_get_distinct_temp_files_even_in_one_burst() {
        // r14 item 7: every concurrent publication must stage in its OWN temp
        // file. The old fixed `<name>.json.tmp` gave every lane the same path,
        // so staging writes/cleanups raced. A tight concurrent burst through
        // unique_temp_path must yield zero collisions (the nonce guarantees
        // it within a process; the pid disambiguates across processes).
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("base.json");
        let threads = 16;
        let per = 200;
        let paths: std::collections::HashSet<_> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..threads {
                handles.push(scope.spawn(|| {
                    (0..per)
                        .map(|_| unique_temp_path(&target))
                        .collect::<Vec<_>>()
                }));
            }
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(
            paths.len(),
            threads * per,
            "concurrent report publications collide on a shared temp file"
        );
        // Every staging file lives next to the report and is a distinct hidden
        // .tmp name (so no lane ever removes another lane's bytes).
        for path in &paths {
            assert!(path.parent() == Some(temporary.path()));
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with(".base.json.") && name.ends_with(".tmp"),
                "{name}"
            );
        }
    }

    #[test]
    fn concurrent_publishers_of_the_same_tuple_all_leave_one_record_and_no_temp() {
        // r15 item 8: many lanes publish the SAME immutable identity at once.
        // The create-new link/rename must (a) let every publisher succeed (the
        // record is content-addressed), (b) leave exactly ONE final file whose
        // contents are the shared tuple, and (c) leave NO staging temp files
        // behind (a loser must clean up its own temp, never the winner's).
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/concurrent";
        let report = test_report("wt/concurrent", "base-sha", "head-sha");
        let path = immutable_report_path(repo, reference, "merge-sha").unwrap();

        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..16 {
                let report = &report;
                handles.push(scope.spawn(move || save(repo, reference, report)));
            }
            for handle in handles {
                handle.join().unwrap().expect("every publisher succeeds");
            }
        });

        // Exactly one published record, content-addressed and correct.
        let files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(files.len(), 1, "concurrent publishers left {files:?}");
        let published: MergeReport =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(published.base.as_deref(), Some("base-sha"));
        assert_eq!(published.head.as_deref(), Some("head-sha"));
        assert_eq!(published.merged.as_deref(), Some("merge-sha"));

        // No staging files left anywhere under the report dir.
        let leftover: Vec<_> = walkdir(&report_dir(repo, reference).unwrap())
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "temp staging files leaked: {leftover:?}"
        );
    }

    #[test]
    fn concurrent_publishers_of_different_tuples_do_not_replace_each_other() {
        // r15 item 8: two DIFFERENT (base, head, merged) captures for the same
        // merge id must not overwrite each other; the loser keeps its file and
        // the conflict is surfaced (never a silent last-writer-wins).
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        super::super::git(repo, &["init", "-b", "main"]).unwrap();
        let reference = "refs/heads/wt/diverge";
        let mk = |base: &str| test_report("wt/diverge", base, &format!("{base}-head"));
        let a = mk("base-a");
        let b = mk("base-b");
        // Seed A, then B must fail (different tuple for the same identity).
        save(repo, reference, &a).unwrap();
        let b_result = save(repo, reference, &b);
        assert!(b_result.is_err(), "a different tuple must not replace A");
        let persisted: MergeReport = serde_json::from_str(
            &std::fs::read_to_string(immutable_report_path(repo, reference, "merge-sha").unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.base.as_deref(), Some("base-a"));
    }

    fn test_report(branch: &str, base: &str, head: &str) -> MergeReport {
        let args = MergeArgs {
            branch: Some(branch.into()),
            ..MergeArgs::default_for_test()
        };
        let mut report = new_report(branch, &args);
        report.exit_code = 0;
        report.status = "verified".into();
        report.base = Some(base.into());
        report.head = Some(head.into());
        report.merged = Some("merge-sha".into());
        report
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(root) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    out.extend(walkdir(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }
}
