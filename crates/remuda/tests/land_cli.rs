//! `remuda land` on a lane that cannot push: the lane verifies, the project
//! home host fetches the pinned merge and compare-and-swap pushes `main`.
//!
//! Unlike `gate_cli.rs`, the lane and home hosts here are **real Nodes** over
//! real git repositories, so the fetch and the push actually happen:
//!
//! * `origin.git` — a bare repo standing in for the project remote.
//! * `lane/` — the lane checkout. Its `origin` remote is a path the lane may
//!   read but must never push to; the test asserts it does not.
//! * `home/` — the home host's checkout, the only one with a writable
//!   `origin`, and the one that reaches the lane as a plain remote.
//!
//! Covers the two outcomes that matter for D-034: `main` moves exactly once on
//! a clean land, and a base that moved re-queues a verify instead of pushing.

use anyhow::Result;
use remuda_node::{DevNode, DevServerConfig};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A project remote, a credential-less lane checkout, and a home checkout.
struct Fixture {
    _dir: tempfile::TempDir,
    origin: PathBuf,
    lane: PathBuf,
    home: PathBuf,
    /// The verified merge commit, pinned in the lane repo.
    merge_sha: String,
    /// `main` at verification time (the merge's first parent).
    base_sha: String,
    job_id: String,
}

impl Fixture {
    /// Build the three repos and leave a verified merge pinned on the lane the
    /// way a passing `gate.run` does.
    fn build(job_id: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let origin = root.join("origin.git");
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&origin)
            .status()?;

        // Seed the remote with one commit on main and a feature branch.
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed)?;
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "T"]);
        std::fs::write(seed.join("file.txt"), "base\n")?;
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(&seed, &["checkout", "-q", "-b", "wt/land/task"]);
        std::fs::write(seed.join("work.txt"), "work\n")?;
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "task work"]);
        git(
            &seed,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(&seed, &["push", "-q", "origin", "main", "wt/land/task"]);

        // The lane checkout, and the home checkout.
        for path in [root.join("lane"), root.join("home")] {
            std::process::Command::new("git")
                .current_dir(root)
                .args(["clone", "-q"])
                .arg(&origin)
                .arg(&path)
                .status()?;
            git(&path, &["config", "user.email", "t@example.com"]);
            git(&path, &["config", "user.name", "T"]);
        }
        let lane = root.join("lane");
        let home = root.join("home");
        // The home host reaches the lane repo as a plain remote; this stands in
        // for the operator's existing ssh alias (`fetchRemote`).
        git(
            &home,
            &["remote", "add", "lane-alias", &lane.to_string_lossy()],
        );

        // Build the merge on the lane the way `merge --onto --gate` does: in a
        // scratch worktree that is then removed, leaving only the pin.
        let base_sha = git(&lane, &["rev-parse", "main"]);
        let scratch = root.join("scratch");
        git(
            &lane,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &scratch.to_string_lossy(),
                &base_sha,
            ],
        );
        git(
            &scratch,
            &[
                "merge",
                "-q",
                "--no-ff",
                "--no-edit",
                "-m",
                "merge: wt/land/task",
                "origin/wt/land/task",
            ],
        );
        let merge_sha = git(&scratch, &["rev-parse", "HEAD"]);
        git(
            &lane,
            &["worktree", "remove", "--force", &scratch.to_string_lossy()],
        );
        git(
            &lane,
            &[
                "update-ref",
                &remuda_protocol::gate_merge_ref(job_id),
                &merge_sha,
            ],
        );

        Ok(Self {
            _dir: dir,
            origin,
            lane,
            home,
            merge_sha,
            base_sha,
            job_id: job_id.to_owned(),
        })
    }

    /// `main` on the project remote.
    fn origin_main(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }

    /// `gate.land` params for the home host.
    fn land_params(&self, base_sha: &str) -> Value {
        json!({
            "jobId": self.job_id,
            "repoPath": self.home.to_string_lossy(),
            "branch": "wt/land/task",
            "baseBranch": "main",
            "pushRemote": "origin",
            "fetchRemote": "lane-alias",
            "mergeRef": remuda_protocol::gate_merge_ref(&self.job_id),
            "mergeSha": self.merge_sha,
            "baseSha": base_sha,
        })
    }
}

/// A Node rooted at one workspace, standing in for a host.
fn node_at(root: &Path) -> Result<DevNode> {
    let config = DevServerConfig::loopback(0)
        .with_workspace_root(root.to_path_buf())
        .with_workspace_roots(vec![root.to_path_buf()])
        .with_workspace_registry(root.to_path_buf());
    Ok(DevNode::new(&config)?)
}

/// The lane holds no credential for the project remote, so the home host does
/// the push: `main` moves exactly once, to the verified merge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn home_host_lands_a_verify_from_a_credential_less_lane() -> Result<()> {
    let fixture = Fixture::build("gjb_land1")?;
    let home = node_at(&fixture.home)?;

    // Precondition: the lane never pushed. The remote is still at the base,
    // and the merge exists *only* in the lane repo.
    assert_eq!(fixture.origin_main(), fixture.base_sha);
    let has_merge = std::process::Command::new("git")
        .current_dir(&fixture.home)
        .args([
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", fixture.merge_sha),
        ])
        .output()?
        .status
        .success();
    assert!(
        !has_merge,
        "the merge must start out only in the lane repo, so the land really fetches it"
    );

    let result = home
        .dispatch_gate_rpc("gate.land", &fixture.land_params(&fixture.base_sha))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(
        result["status"].as_str(),
        Some("landed"),
        "the home host should push: {result}"
    );

    // main is exactly the verified merge, and its first parent is the base the
    // gate verified against.
    assert_eq!(
        fixture.origin_main(),
        fixture.merge_sha,
        "origin/main must be the verified merge"
    );
    assert_eq!(
        git(
            &fixture.home,
            &["rev-parse", &format!("{}^1", fixture.merge_sha)]
        ),
        fixture.base_sha,
        "the landed merge's first parent must be the verified base"
    );
    Ok(())
}

/// A base that moved after verification must refuse: no push, and a
/// `base-moved` verdict the Hub turns into a re-queued verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_moved_base_refuses_the_push_and_reports_base_moved() -> Result<()> {
    let fixture = Fixture::build("gjb_land2")?;
    let home = node_at(&fixture.home)?;

    // Someone else lands first: main advances past the verified base.
    let other = fixture._dir.path().join("other");
    std::process::Command::new("git")
        .current_dir(fixture._dir.path())
        .args(["clone", "-q"])
        .arg(&fixture.origin)
        .arg(&other)
        .status()?;
    git(&other, &["config", "user.email", "t@example.com"]);
    git(&other, &["config", "user.name", "T"]);
    std::fs::write(other.join("other.txt"), "other\n")?;
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "other work"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let moved_main = fixture.origin_main();
    assert_ne!(moved_main, fixture.base_sha, "main should have moved");

    let result = home
        .dispatch_gate_rpc("gate.land", &fixture.land_params(&fixture.base_sha))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(
        result["status"].as_str(),
        Some("base-moved"),
        "a moved base must refuse: {result}"
    );
    assert_eq!(
        result["currentMainSha"].as_str(),
        Some(moved_main.as_str()),
        "the refusal reports where main actually is: {result}"
    );
    // Nothing was pushed: main is exactly where the other lander left it, and
    // it is emphatically not the verified merge.
    assert_eq!(
        fixture.origin_main(),
        moved_main,
        "a refused land must not move main"
    );
    assert_ne!(fixture.origin_main(), fixture.merge_sha);
    Ok(())
}

/// The push must be the verified merge and nothing else: if the lane's pinned
/// ref no longer matches the sha that passed the gate, refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repinned_lane_ref_is_refused_rather_than_pushed() -> Result<()> {
    let fixture = Fixture::build("gjb_land3")?;
    let home = node_at(&fixture.home)?;

    // The lane re-pins its ref at a different commit than the one verified.
    let other_sha = git(&fixture.lane, &["rev-parse", "origin/wt/land/task"]);
    git(
        &fixture.lane,
        &[
            "update-ref",
            &remuda_protocol::gate_merge_ref(&fixture.job_id),
            &other_sha,
        ],
    );

    let result = home
        .dispatch_gate_rpc("gate.land", &fixture.land_params(&fixture.base_sha))
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(
        result["status"].as_str(),
        Some("failed"),
        "an unverified commit must never be pushed: {result}"
    );
    assert_eq!(
        fixture.origin_main(),
        fixture.base_sha,
        "main must not move when the pin does not match"
    );
    Ok(())
}

/// A land whose home host is unreachable must not report success. Exercised
/// through a `fetchRemote` that cannot serve the ref.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfetchable_merge_fails_without_touching_main() -> Result<()> {
    let fixture = Fixture::build("gjb_land4")?;
    let home = node_at(&fixture.home)?;
    let mut params = fixture.land_params(&fixture.base_sha);
    params["fetchRemote"] = json!("no-such-remote");

    let result = home
        .dispatch_gate_rpc("gate.land", &params)
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(
        result["status"].as_str(),
        Some("failed"),
        "an unreachable lane is a failure, never a silent success: {result}"
    );
    assert_eq!(
        fixture.origin_main(),
        fixture.base_sha,
        "main must be untouched"
    );
    // The failure names the fetch, so an operator knows which half broke.
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("fetch"), "{result}");
    Ok(())
}
