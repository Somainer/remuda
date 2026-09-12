//! Real local Git refs, worktrees and pushes; only gate executables are stubbed.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

const GATE: &str = include_str!("../../../scripts/ci/gate.sh");
const AFFECTED: &str = include_str!("../../../scripts/ci/affected.py");
const STUB: &str = include_str!("fixtures/merge-gate-stub.py");

fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = git_command(repo).args(args).output().expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn commit_file(repo: &Path, path: &str, content: &str) -> String {
    let file = repo.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, content).unwrap();
    git(repo, &["add", "--", path]);
    git(repo, &["commit", "-m", "fixture change", "--", path]);
    git(repo, &["rev-parse", "HEAD"])
}

struct Repo {
    _keep: tempfile::TempDir,
    root: PathBuf,
    origin: PathBuf,
    source: PathBuf,
    stub: PathBuf,
    trace: PathBuf,
    base: String,
    branch_commit: String,
}

impl Repo {
    fn new() -> Self {
        let keep = tempfile::tempdir().unwrap();
        // Git canonicalizes the macOS temporary-directory symlink as well.
        let parent = keep.path().canonicalize().unwrap();
        let root = parent.join("repo with spaces");
        let origin = parent.join("origin.git");
        let source = parent.join("source worktree");
        let stub = parent.join("gate stub.py");
        let trace = parent.join("trace.jsonl");
        fs::create_dir_all(root.join("scripts/ci")).unwrap();
        fs::create_dir_all(root.join("web")).unwrap();
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", "-b", "main"]);
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "merge test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("scripts/ci/gate.sh"), GATE).unwrap();
        fs::write(root.join("scripts/ci/affected.py"), AFFECTED).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\", \"crates/app\", \"crates/other\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        let mut rust_files = vec!["Cargo.toml".to_owned(), "Cargo.lock".to_owned()];
        for name in ["core", "app", "other"] {
            fs::create_dir_all(root.join(format!("crates/{name}/src"))).unwrap();
            let manifest = format!("crates/{name}/Cargo.toml");
            let library = format!("crates/{name}/src/lib.rs");
            fs::write(root.join(&manifest), format!(
                "[package]\nname = \"gate-{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{}",
                if name == "app" { "[dependencies]\ngate-core = { path = \"../core\" }\n" } else { "" }
            )).unwrap();
            fs::write(root.join(&library), "pub fn fixture() {}\n").unwrap();
            rust_files.extend([manifest, library]);
        }
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("web/.keep"), "fixture\n").unwrap();
        fs::write(root.join(".gitignore"), "/data/tmp/\n/target-gate/\n").unwrap();
        fs::write(root.join("shared file.txt"), "base\n").unwrap();
        let initial = [
            ".gitignore",
            "shared file.txt",
            "scripts/ci/gate.sh",
            "scripts/ci/affected.py",
            "web/.keep",
        ];
        let mut add = vec!["add", "--"];
        add.extend(initial);
        add.extend(rust_files.iter().map(String::as_str));
        git(&root, &add);
        let mut commit = vec!["commit", "-m", "fixture base", "--"];
        commit.extend(initial);
        commit.extend(rust_files.iter().map(String::as_str));
        git(&root, &commit);
        let base = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&root, &["push", "origin", "main"]);
        git(
            &root,
            &["worktree", "add", "-b", "topic", source.to_str().unwrap()],
        );
        let branch_commit = commit_file(&source, "shared file.txt", "branch\n");
        fs::write(&stub, STUB).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _keep: keep,
            root,
            origin,
            source,
            stub,
            trace,
            base,
            branch_commit,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("REMUDA_MERGE_GATE_COMMAND", &self.stub)
            .env("REMUDA_TEST_GATE_TRACE", &self.trace)
            // Merge must ignore the worker's inherited Cargo target.
            .env("CARGO_TARGET_DIR", self.root.join("worker-target"))
            .env("CARGO_INCREMENTAL", "1");
        command
    }

    fn merge(&self, flags: &[&str], env: &[(&str, &str)]) -> (Output, Value) {
        let output = self
            .command()
            .args(["merge", "topic", "--json"])
            .args(flags)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "JSON {error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, value)
    }

    fn main(&self) -> String {
        git(&self.root, &["rev-parse", "main"])
    }

    fn remote_main(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_cleaned(&self) {
        let list = git(&self.root, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            list.lines()
                .filter(|line| line.starts_with("worktree "))
                .count(),
            2,
            "{list}"
        );
        let tmp = self.root.join("data/tmp");
        if tmp.exists() {
            assert_eq!(fs::read_dir(tmp).unwrap().count(), 0);
        }
    }
}

fn step<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["name"] == name)
        .unwrap()
}

fn assert_exit(output: &Output, report: &Value, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["exitCode"], code);
    assert!(
        report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|step| step["durationMs"].is_u64())
    );
}

#[test]
fn affected_gate_tests_only_changed_crates_and_reverse_dependencies() {
    let repo = Repo::new();
    git(&repo.source, &["reset", "--hard", &repo.base]);
    commit_file(
        &repo.source,
        "crates/core/src/lib.rs",
        "pub fn changed() {}\n",
    );
    let (output, report) = repo.merge(&["--gate", "--no-push"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["affected"], true);
    assert_eq!(
        step(&report, "cargo-test")["crates"],
        json!(["gate-app", "gate-core"])
    );
    assert_eq!(step(&report, "cargo-test")["status"], "ok");
    repo.assert_cleaned();
}

#[test]
fn full_gate_reports_all_workspace_crates() {
    let repo = Repo::new();
    git(&repo.source, &["reset", "--hard", &repo.base]);
    commit_file(
        &repo.source,
        "crates/app/src/lib.rs",
        "pub fn changed() {}\n",
    );
    let (output, report) = repo.merge(&["--gate", "--full", "--no-push"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["affected"], false);
    assert_eq!(
        step(&report, "cargo-test")["crates"],
        json!(["gate-app", "gate-core", "gate-other"])
    );
    repo.assert_cleaned();
}

#[test]
fn docs_only_gate_skips_tests_but_keeps_the_other_rust_checks() {
    let repo = Repo::new();
    git(&repo.source, &["reset", "--hard", &repo.base]);
    commit_file(&repo.source, "docs/note.md", "documentation\n");
    let (output, report) = repo.merge(&["--gate", "--no-push"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(step(&report, "cargo-test")["status"], "skipped");
    assert_eq!(step(&report, "cargo-test")["crates"], json!([]));
    for name in ["secret-scan", "cargo-fmt", "cargo-check", "cargo-clippy"] {
        assert_eq!(step(&report, name)["status"], "ok");
    }
    assert!(!repo.trace().iter().any(|line| line["step"] == "cargo-test"));
    repo.assert_cleaned();
}

#[test]
fn merge_pushes_verified_no_ff_commit_and_preserves_worker_edits() {
    let repo = Repo::new();
    fs::write(
        repo.source.join("shared file.txt"),
        "unstaged worker edit\n",
    )
    .unwrap();
    fs::write(repo.source.join("untracked.txt"), "worker data\n").unwrap();
    let (output, report) = repo.merge(&["--gate"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["status"], "ok");
    assert_eq!(report["expectedMain"], repo.base);
    assert_eq!(report["source"], repo.branch_commit);
    assert_eq!(report["merged"], repo.main());
    assert_eq!(repo.main(), repo.remote_main());
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", "main"]),
        format!("{} {}", repo.base, repo.branch_commit)
    );
    assert_eq!(report["mainUpdated"], true);
    assert_eq!(report["pushed"], true);
    assert_eq!(report["gateOverride"], true);
    assert_eq!(report["web"], false);
    assert_eq!(step(&report, "web-install")["status"], "skipped");
    assert_eq!(step(&report, "cleanup")["status"], "ok");
    let trace = repo.trace();
    assert_eq!(
        trace
            .iter()
            .map(|event| event["step"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "secret-scan",
            "cargo-fmt",
            "cargo-check",
            "cargo-clippy",
            "cargo-test"
        ]
    );
    assert!(trace.iter().all(|event| event["incremental"] == "0"));
    assert!(
        trace
            .iter()
            .all(|event| event["target"] == repo.root.join("target-gate").to_str().unwrap())
    );
    assert!(trace.iter().all(|event| {
        Path::new(event["cwd"].as_str().unwrap()).starts_with(repo.root.join("data/tmp"))
    }));
    assert_eq!(
        fs::read_to_string(repo.source.join("shared file.txt")).unwrap(),
        "unstaged worker edit\n"
    );
    assert!(repo.source.join("untracked.txt").exists());
    repo.assert_cleaned();
}

#[test]
fn test_retry_is_reported_once_and_forced_web_runs_in_web_directory_without_push() {
    let repo = Repo::new();
    let (output, report) = repo.merge(
        &[
            "--gate",
            "--no-push",
            "--web",
            "--target-dir",
            "chosen-target",
        ],
        &[("REMUDA_TEST_GATE_RETRY", "cargo-test")],
    );
    assert_exit(&output, &report, 0);
    assert_eq!(repo.remote_main(), repo.base);
    assert_eq!(report["pushed"], false);
    assert_eq!(step(&report, "cargo-test")["attempts"], 2);
    assert_eq!(step(&report, "cargo-test")["retried"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("retried"));
    let trace = repo.trace();
    assert_eq!(
        trace
            .iter()
            .map(|event| event["step"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "secret-scan",
            "cargo-fmt",
            "cargo-check",
            "cargo-clippy",
            "cargo-test",
            "cargo-test",
            "web-install",
            "web-build",
            "web-test"
        ]
    );
    assert!(
        trace
            .iter()
            .all(|event| event["target"] == repo.root.join("chosen-target").to_str().unwrap())
    );
    assert!(
        trace[6..]
            .iter()
            .all(|event| Path::new(event["cwd"].as_str().unwrap()).ends_with("worktree/web"))
    );
    repo.assert_cleaned();
}

#[test]
fn web_changes_automatically_enable_web_checks() {
    let repo = Repo::new();
    commit_file(&repo.source, "web/a new file.txt", "web branch change\n");
    let (output, report) = repo.merge(&["--gate", "--no-push"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["web"], true);
    assert_eq!(step(&report, "web-test")["status"], "ok");
    repo.assert_cleaned();
}

#[test]
fn gate_failures_stop_in_order_and_never_update_main() {
    for (failure, attempts) in [("cargo-check", 1), ("cargo-test", 2)] {
        let repo = Repo::new();
        let (output, report) =
            repo.merge(&["--gate", "--web"], &[("REMUDA_TEST_GATE_FAIL", failure)]);
        assert_exit(&output, &report, 1);
        assert_eq!(report["mainUpdated"], false);
        assert_eq!(step(&report, failure)["status"], "failed");
        assert_eq!(step(&report, failure)["attempts"], attempts);
        assert_eq!(step(&report, "web-install")["status"], "skipped");
        assert_eq!(repo.main(), repo.base);
        assert_eq!(repo.remote_main(), repo.base);
        repo.assert_cleaned();
    }
}

#[test]
fn conflicts_report_file_list_and_exit_two() {
    let repo = Repo::new();
    let expected = commit_file(&repo.root, "shared file.txt", "main change\n");
    let (output, report) = repo.merge(&["--gate"], &[]);
    assert_exit(&output, &report, 2);
    assert_eq!(report["conflicts"], json!(["shared file.txt"]));
    assert_eq!(report["status"], "conflict");
    assert_eq!(repo.main(), expected);
    assert!(repo.trace().is_empty());
    repo.assert_cleaned();
}

#[test]
fn concurrent_main_update_loses_cas_without_pushing() {
    let repo = Repo::new();
    let tree = git(&repo.root, &["rev-parse", "main^{tree}"]);
    let concurrent = git(
        &repo.root,
        &[
            "commit-tree",
            &tree,
            "-p",
            &repo.base,
            "-m",
            "concurrent coordinator",
        ],
    );
    let (output, report) = repo.merge(&["--gate"], &[("REMUDA_TEST_GATE_CAS", &concurrent)]);
    assert_exit(&output, &report, 3);
    assert_eq!(report["status"], "cas_lost");
    assert_eq!(report["mainUpdated"], false);
    assert_eq!(repo.main(), concurrent);
    assert_eq!(repo.remote_main(), repo.base);
    repo.assert_cleaned();
}

#[test]
fn staged_source_changes_are_refused_before_creating_a_worktree() {
    let repo = Repo::new();
    fs::write(repo.source.join("staged.txt"), "unfinished\n").unwrap();
    git(&repo.source, &["add", "--", "staged.txt"]);
    let (output, report) = repo.merge(&["--gate"], &[]);
    assert_exit(&output, &report, 1);
    assert!(report["error"].as_str().unwrap().contains("staged changes"));
    assert_eq!(
        git(&repo.source, &["diff", "--cached", "--name-only"]),
        "staged.txt"
    );
    assert_eq!(repo.main(), repo.base);
    assert!(!repo.root.join("data/tmp").exists());
    assert!(repo.trace().is_empty());
}

#[test]
fn rebase_and_merge_in_progress_are_refused_even_with_detached_head() {
    for operation in ["rebase", "merge"] {
        let repo = Repo::new();
        commit_file(&repo.root, "shared file.txt", "conflicting main\n");
        let progress = git_command(&repo.source)
            .args([operation, "main"])
            .output()
            .unwrap();
        assert!(!progress.status.success());
        if operation == "rebase" {
            assert_eq!(
                git(&repo.source, &["rev-parse", "--abbrev-ref", "HEAD"]),
                "HEAD"
            );
        }
        let (output, report) = repo.merge(&["--gate"], &[]);
        assert_exit(&output, &report, 1);
        assert!(
            report["error"]
                .as_str()
                .unwrap()
                .contains("rebase/merge in progress"),
            "{report}"
        );
        assert!(!repo.root.join("data/tmp").exists());
        assert!(repo.trace().is_empty());
        git(&repo.source, &[operation, "--abort"]);
    }
}

#[test]
fn dry_run_does_not_fetch_create_worktrees_or_run_gate_commands() {
    let repo = Repo::new();
    commit_file(&repo.source, "web/new.txt", "web change\n");
    git(
        &repo.root,
        &["remote", "set-url", "origin", "missing-remote"],
    );
    let fetch_head = repo.root.join(".git/FETCH_HEAD");
    assert!(!fetch_head.exists());
    let (output, report) = repo.merge(&["--dry-run"], &[]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["web"], true);
    assert_eq!(step(&report, "fetch")["status"], "planned");
    assert_eq!(step(&report, "web-install")["status"], "planned");
    assert_eq!(step(&report, "update-main")["status"], "planned");
    assert_eq!(report["mainUpdated"], false);
    assert_eq!(report["merged"], Value::Null);
    assert_eq!(repo.main(), repo.base);
    assert!(!fetch_head.exists());
    assert!(!repo.root.join("data/tmp").exists());
    assert!(repo.trace().is_empty());
}

#[test]
fn push_failure_reports_that_local_main_was_already_advanced() {
    let repo = Repo::new();
    git(
        &repo.root,
        &["remote", "set-url", "--push", "origin", "missing-remote"],
    );
    let (output, report) = repo.merge(&["--gate"], &[]);
    assert_exit(&output, &report, 1);
    assert_eq!(report["mainUpdated"], true);
    assert_eq!(report["pushed"], false);
    assert_eq!(step(&report, "push")["status"], "failed");
    assert_eq!(report["merged"], repo.main());
    assert_eq!(repo.remote_main(), repo.base);
    repo.assert_cleaned();
}

#[test]
fn gate_cannot_publish_a_tree_it_mutated() {
    let repo = Repo::new();
    let (output, report) = repo.merge(&["--gate"], &[("REMUDA_TEST_GATE_MUTATE", "1")]);
    assert_exit(&output, &report, 1);
    assert_eq!(step(&report, "verify-tree")["status"], "failed");
    assert_eq!(repo.main(), repo.base);
    repo.assert_cleaned();
}

#[test]
fn cleanup_removes_git_registration_when_data_directory_is_a_symlink() {
    let repo = Repo::new();
    let data = repo.root.parent().unwrap().join("external data");
    fs::create_dir(&data).unwrap();
    std::os::unix::fs::symlink(&data, repo.root.join("data")).unwrap();
    let (output, report) = repo.merge(&["--gate", "--no-push"], &[]);
    assert_exit(&output, &report, 0);
    repo.assert_cleaned();
    assert_eq!(fs::read_dir(data.join("tmp")).unwrap().count(), 0);
}

#[test]
fn mcp_merge_returns_the_same_structured_result_and_marks_failures() {
    for fail in [false, true] {
        let repo = Repo::new();
        let mut command = repo.command();
        command
            .args(["mcp", "--hub", "http://127.0.0.1:1"])
            .env("REMUDA_TOKEN", "test-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if fail {
            command.env("REMUDA_TEST_GATE_FAIL", "cargo-check");
        }
        let mut child = command.spawn().unwrap();
        let request = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
            "name":"remuda_merge", "arguments":{"branch":"topic", "gate":true, "noPush":true}
        }});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rpc: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rpc["result"]["isError"], fail);
        let report = &rpc["result"]["structuredContent"];
        assert_eq!(report["exitCode"], if fail { 1 } else { 0 });
        assert_eq!(report["mainUpdated"], !fail);
        assert_eq!(report["pushed"], false);
        let text: Value =
            serde_json::from_str(rpc["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(&text, report);
        repo.assert_cleaned();
    }
}
