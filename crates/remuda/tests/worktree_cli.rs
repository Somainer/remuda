//! `remuda worktree create` against an isolated git repository.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

fn git(repo: &Path, args: &[&str]) {
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

/// A repo with one empty commit on `main`, created inside `parent`.
fn init_repo(parent: &Path) -> std::path::PathBuf {
    let repo = parent.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    git(&repo, &["init", "-q"]);
    // `git init -b main` needs git >= 2.28; set HEAD directly so this runs on
    // older hosts too.
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["commit", "--allow-empty", "-m", "init"]);
    repo
}

fn create(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(["worktree", "create"])
        .args(args)
        .args(["--repo", repo.to_str().expect("utf8")])
        .output()
        .expect("worktree create")
}

#[test]
fn worktree_create_adds_branch_and_prints_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let output = create(&repo, &["agent1", "--base", "main"]);
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
    // The worktree lands beside the repo, under `remuda-wt/`.
    let path = Path::new(value["path"].as_str().expect("path"));
    assert!(path.join(".git").exists(), "{value}");
    assert_eq!(
        path.parent().and_then(Path::file_name).unwrap(),
        "remuda-wt",
        "{value}"
    );
}

/// `security-review-2.md` M4: an explicit `--path` is honoured only inside the
/// worktree root, so a checkout cannot be written to an attacker-chosen place.
#[test]
fn worktree_create_rejects_a_path_outside_the_worktree_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    // Inside the root: accepted.
    let inside = dir.path().join("remuda-wt").join("agent-ok");
    let output = create(&repo, &["agent1", "--path", inside.to_str().expect("utf8")]);
    assert!(
        output.status.success(),
        "a path inside the worktree root was rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Outside the root: refused, and nothing is created.
    let outside = dir.path().join("evil");
    let output = create(
        &repo,
        &["agent2", "--path", outside.to_str().expect("utf8")],
    );
    assert!(
        !output.status.success(),
        "a path outside the worktree root was accepted"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("worktree path rejected"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!outside.exists(), "the rejected directory was created");
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let keep = tempfile::tempdir().unwrap();
    let repo = keep.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    // `git init -b main` needs git >= 2.28; set HEAD directly instead so this
    // runs on older hosts too (matches `init_repo` above).
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["commit", "--allow-empty", "-m", "fixture"]);
    (keep, repo)
}

fn worktree(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .current_dir(repo)
        .arg("worktree")
        .args(args)
        .output()
        .unwrap()
}

fn result(output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn ls_and_rm_preserve_branches_and_protect_dirty_locked_current_and_main_worktrees() {
    let (_keep, repo) = fixture();
    let tree = result(worktree(&repo, &["create", "worker"]));
    let path = std::path::Path::new(tree["path"].as_str().unwrap());
    let list = result(worktree(&repo, &["ls", "--json"]));
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert_eq!(list["items"][1]["name"], "worker");
    assert_eq!(list["items"][1]["branch"], tree["branch"]);
    let main = worktree(&repo, &["rm", repo.to_str().unwrap(), "--force"]);
    assert!(!main.status.success());
    assert!(String::from_utf8_lossy(&main.stderr).contains("primary, current, or main"));
    let current = worktree(path, &["rm", "worker", "--force"]);
    assert!(!current.status.success());
    std::fs::write(
        path.join("user data.txt"),
        "keep unless explicitly forced\n",
    )
    .unwrap();
    assert!(!worktree(&repo, &["rm", "worker"]).status.success());
    assert!(path.join("user data.txt").exists());
    git(&repo, &["worktree", "lock", path.to_str().unwrap()]);
    let locked = worktree(&repo, &["rm", "worker", "--force"]);
    assert!(!locked.status.success());
    assert!(String::from_utf8_lossy(&locked.stderr).contains("locked"));
    git(&repo, &["worktree", "unlock", path.to_str().unwrap()]);
    let removed = result(worktree(&repo, &["rm", "worker", "--force"]));
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["branchDeleted"], false);
    assert!(!path.exists());
    git(
        &repo,
        &[
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", tree["branch"].as_str().unwrap()),
        ],
    );
    let list = result(worktree(&repo, &["ls", "--json"]));
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
}

#[test]
fn prune_only_drops_missing_unlocked_registrations_and_reconciles_the_catalog() {
    let (_keep, repo) = fixture();
    let gone = result(worktree(&repo, &["create", "gone"]));
    let locked = result(worktree(&repo, &["create", "locked"]));
    let present = result(worktree(&repo, &["create", "present"]));
    let lock_path = locked["path"].as_str().unwrap();
    git(&repo, &["worktree", "lock", lock_path]);
    std::fs::remove_dir_all(gone["path"].as_str().unwrap()).unwrap();
    std::fs::remove_dir_all(lock_path).unwrap();
    let preview = result(worktree(&repo, &["prune", "--dry-run"]));
    assert_eq!(preview["catalogRemoved"], serde_json::json!(["gone"]));
    assert_eq!(
        result(worktree(&repo, &["ls", "--json"]))["items"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let pruned = result(worktree(&repo, &["prune"]));
    assert_eq!(pruned["catalogRemoved"], serde_json::json!(["gone"]));
    let list = result(worktree(&repo, &["ls", "--json"]));
    assert_eq!(list["items"].as_array().unwrap().len(), 3);
    assert!(std::path::Path::new(present["path"].as_str().unwrap()).exists());
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "locked" && item["locked"] == true)
    );
}

#[test]
fn rm_refuses_an_active_merge_even_with_force() {
    let (_keep, repo) = fixture();
    let tree = result(worktree(&repo, &["create", "worker"]));
    let path = std::path::Path::new(tree["path"].as_str().unwrap());
    let output = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let admin = String::from_utf8(output.stdout).unwrap();
    std::fs::write(
        std::path::Path::new(admin.trim()).join("MERGE_HEAD"),
        "fixture state\n",
    )
    .unwrap();
    let output = worktree(&repo, &["rm", "worker", "--force"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("merge/rebase in progress"));
    assert!(path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_worktree_rm_uses_the_same_safety_checks_and_preserves_the_branch() {
    use std::io::Write;
    use std::process::Stdio;
    let (keep, repo) = fixture();
    let hub = remuda_hub::spawn(remuda_hub::HubConfig::for_test(keep.path().join("hub")))
        .await
        .unwrap();
    let token = hub.mint_device_token("worktree-coordinator").await.unwrap();
    let hub_url = format!("http://{}", hub.addr);
    let tree = result(worktree(&repo, &["create", "worker"]));
    let path = std::path::Path::new(tree["path"].as_str().unwrap());
    std::fs::write(path.join("unfinished.txt"), "unfinished\n").unwrap();
    let mut child = Command::new(bin())
        .current_dir(&repo)
        .args(["mcp", "--hub", &hub_url, "--token", &token])
        .env_remove("REMUDA_INSTANCE_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for (id, force) in [(1, false), (2, true)] {
        // No `repo` argument: MCP tools resolve against the server's own
        // repository (security-review-2 M4), which is `current_dir` above.
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                "name":"remuda_worktree_rm","arguments":{"name":"worker","force":force}
            }})
        )
        .unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["result"]["isError"], true);
    assert_eq!(frames[1]["result"]["isError"], false);
    let removed: Value =
        serde_json::from_str(frames[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(removed["branchDeleted"], false);
    assert!(!path.exists());
    git(
        &repo,
        &[
            "show-ref",
            "--verify",
            &format!("refs/heads/{}", tree["branch"].as_str().unwrap()),
        ],
    );
}
