//! `remuda worktree create` against an isolated git repository.

use serde_json::Value;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

fn git(repo: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worktree_create_adds_branch_and_prints_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    let path = dir.path().join("agent-wt");
    let output = Command::new(bin())
        .args([
            "worktree",
            "create",
            "agent1",
            "--base",
            "main",
            "--path",
            path.to_str().expect("utf8"),
            "--repo",
            repo.to_str().expect("utf8"),
        ])
        .output()
        .expect("worktree create");
    assert!(
        output.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["name"], "agent1");
    assert!(
        value["branch"]
            .as_str()
            .expect("branch")
            .starts_with("wt/agent1/"),
        "{value}"
    );
    assert!(path.join(".git").exists() || path.exists());
}
