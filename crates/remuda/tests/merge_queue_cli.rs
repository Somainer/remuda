//! Integration tests for --onto / --land and the optimistic --queue.
//!
//! Real local Git refs, worktrees and two temp repos (coordinator checkout +
//! bare origin); only the gate executable is stubbed.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const GATE: &str = include_str!("../../../scripts/ci/gate.sh");
const AFFECTED: &str = include_str!("../../../scripts/ci/affected.py");
const GATE_SUPERVISOR: &str = include_str!("../../../scripts/ci/gate_supervisor.py");
const QUEUE_STUB: &str = include_str!("fixtures/merge-queue-gate-stub.py");

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

struct QueueRepo {
    _keep: tempfile::TempDir,
    root: PathBuf,
    work_a: PathBuf,
    work_b: PathBuf,
    trace: PathBuf,
    base: String,
}

impl QueueRepo {
    fn new() -> Self {
        let keep = tempfile::tempdir().unwrap();
        let parent = keep.path().canonicalize().unwrap();
        let root = parent.join("queuerepo");
        let origin = parent.join("origin.git");
        let work_a = parent.join("work-a");
        let work_b = parent.join("work-b");
        let stub = parent.join("queue-gate.py");
        let trace = parent.join("trace.jsonl");
        fs::create_dir_all(root.join("scripts/ci")).unwrap();
        fs::create_dir_all(root.join("web/src/lib")).unwrap();
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", "-b", "main"]);
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "merge queue test"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("scripts/ci/gate.sh"), GATE).unwrap();
        fs::write(root.join("scripts/ci/affected.py"), AFFECTED).unwrap();
        fs::write(root.join("scripts/ci/gate_supervisor.py"), GATE_SUPERVISOR).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\"]\nresolver = \"3\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/core/src")).unwrap();
        fs::write(
            root.join("crates/core/Cargo.toml"),
            "[package]\nname=\"gate-core\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        fs::write(root.join("crates/core/src/lib.rs"), "pub fn f() {}\n").unwrap();
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("web/.keep"), "fixture\n").unwrap();
        fs::write(
            root.join("web/src/lib/api.generated.ts"),
            "generated client current\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "/data/tmp/\n/target-gate*/\n").unwrap();
        fs::write(root.join("state.txt"), "ok\n").unwrap();
        let initial = [
            ".gitignore",
            "state.txt",
            "scripts/ci/gate.sh",
            "scripts/ci/affected.py",
            "scripts/ci/gate_supervisor.py",
            "web/.keep",
            "web/src/lib/api.generated.ts",
            "Cargo.toml",
            "Cargo.lock",
            "crates/core/Cargo.toml",
            "crates/core/src/lib.rs",
        ];
        let mut add = vec!["add", "--"];
        add.extend(initial);
        git(&root, &add);
        let mut commit = vec!["commit", "-m", "fixture base", "--"];
        commit.extend(initial);
        git(&root, &commit);
        let base = git(&root, &["rev-parse", "HEAD"]);
        git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&root, &["push", "origin", "main"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "wt/a/work",
                work_a.to_str().unwrap(),
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "wt/b/work",
                work_b.to_str().unwrap(),
            ],
        );
        fs::write(&stub, QUEUE_STUB).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _keep: keep,
            root,
            work_a,
            work_b,
            trace,
            base,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_remuda"));
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("REMUDA_MERGE_GATE_COMMAND", self.stub_path())
            .env("REMUDA_MERGE_BIN", env!("CARGO_BIN_EXE_remuda"))
            .env("REMUDA_TEST_GATE_TRACE", &self.trace);
        command
    }

    fn stub_path(&self) -> PathBuf {
        self.root.parent().unwrap().join("queue-gate.py")
    }

    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = self.command().args(args).output().unwrap();
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

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(&self.trace)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn assert_cleaned(&self) {
        // Gate worktrees live under OS-temp remuda-mq-* scratch roots now;
        // the repository checkout itself must never carry scratch state.
        assert!(
            !self.root.join("data").exists(),
            "merge created scratch inside the repo: {:?}",
            self.root.join("data")
        );
    }
}

fn assert_exit(output: &Output, report: &Value, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{report}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["exitCode"], code);
}

#[test]
fn onto_reports_base_head_tree_and_never_advances_main() {
    let repo = QueueRepo::new();
    let head = commit_file(&repo.work_a, "docs/a.md", "branch a\n");
    let (output, report) = repo.run(&[
        "merge",
        "wt/a/work",
        "--gate",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["status"], "verified");
    assert_eq!(report["base"], repo.base);
    assert_eq!(report["head"], head);
    assert_eq!(report["mainUpdated"], false);
    assert_eq!(report["pushed"], false);
    let merged = report["merged"].as_str().unwrap().to_owned();
    let tree = report["tree"].as_str().unwrap().to_owned();
    assert_ne!(merged, repo.base);
    assert_eq!(tree.len(), 40);
    // Main and origin did not move.
    assert_eq!(repo.main(), repo.base);
    // The merge commit is a real two-parent commit, kept reachable by a pin.
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", &merged]),
        format!("{} {}", repo.base, head)
    );
    let pin = format!("refs/remuda/merge/wt-a-work/{}", repo.base);
    assert_eq!(git(&repo.root, &["rev-parse", &pin]), merged);
    // The gate summary rides on a note; the merge sha itself is unchanged,
    // which is what a speculating lane depends on.
    let note = git(&repo.root, &["notes", "--ref=remuda-gate", "show", &merged]);
    assert!(note.contains("Gate: passed (override=true)"), "{note}");
    // The report is persisted under the common git dir.
    let report_path = repo.root.join(format!(
        ".git/remuda/merge-reports/wt-a-work/{}.json",
        repo.base
    ));
    let persisted: Value = serde_json::from_str(&fs::read_to_string(report_path).unwrap()).unwrap();
    assert_eq!(persisted["merged"], merged);
    assert_eq!(persisted["tree"], tree);
    repo.assert_cleaned();
}

#[test]
fn onto_then_land_fast_forwards_main_to_the_verified_merge() {
    let repo = QueueRepo::new();
    let head = commit_file(&repo.work_a, "docs/a.md", "branch a\n");
    let (_, verify) = repo.run(&[
        "merge",
        "wt/a/work",
        "--gate",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    assert_eq!(verify["status"], "verified");
    assert_eq!(repo.main(), repo.base);
    let merged = verify["merged"].as_str().unwrap().to_owned();
    let (output, report) = repo.run(&[
        "merge",
        "wt/a/work",
        "--land",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["status"], "landed");
    assert_eq!(report["mainUpdated"], true);
    assert_eq!(report["pushed"], false);
    assert_eq!(repo.main(), merged);
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", "main"]),
        format!("{} {}", repo.base, head)
    );
    // --land never runs the gate: its steps are only repository/preflight/land.
    let names: Vec<String> = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["repository", "preflight", "land"], "{names:?}");
    // The pin is gone once the merge is main.
    let pin = format!("refs/remuda/merge/wt-a-work/{}", repo.base);
    let gone = git_command(&repo.root)
        .args(["rev-parse", "--verify", &pin])
        .output()
        .unwrap();
    assert!(!gone.status.success());
    repo.assert_cleaned();
}

#[test]
fn verify_and_land_in_one_invocation_advances_main() {
    let repo = QueueRepo::new();
    commit_file(&repo.work_a, "docs/a2.md", "branch a\n");
    let (output, report) = repo.run(&[
        "merge",
        "wt/a/work",
        "--gate",
        "--land",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    assert_exit(&output, &report, 0);
    assert_eq!(report["status"], "landed");
    assert_eq!(report["mainUpdated"], true);
    assert_eq!(repo.main(), report["merged"].as_str().unwrap());
    repo.assert_cleaned();
}

#[test]
fn land_without_a_report_fails_and_base_moved_exits_two_with_new_main() {
    let repo = QueueRepo::new();
    commit_file(&repo.work_a, "docs/a.md", "branch a\n");
    let (output, report) = repo.run(&[
        "merge",
        "wt/a/work",
        "--land",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    assert_exit(&output, &report, 1);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("run --gate --onto first"),
        "{report}"
    );
    assert_eq!(repo.main(), repo.base);

    // A report exists for the original base; a third party moves main meanwhile.
    let (_, _) = repo.run(&[
        "merge",
        "wt/a/work",
        "--gate",
        "--onto",
        "main",
        "--no-push",
        "--json",
    ]);
    let tree = git(&repo.root, &["rev-parse", "main^{tree}"]);
    let concurrent = git_command(&repo.root)
        .args([
            "commit-tree",
            &tree,
            "-p",
            &repo.base,
            "-m",
            "third party landing",
        ])
        .output()
        .unwrap();
    let concurrent = String::from_utf8(concurrent.stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&repo.root, &["update-ref", "refs/heads/main", &concurrent]);
    let (output, report) = repo.run(&[
        "merge",
        "wt/a/work",
        "--land",
        "--onto",
        &repo.base,
        "--no-push",
        "--json",
    ]);
    assert_exit(&output, &report, 2);
    assert_eq!(report["status"], "base_moved");
    assert_eq!(report["currentMain"], concurrent);
    assert_eq!(repo.main(), concurrent);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&concurrent),
        "new main must be printed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo.assert_cleaned();
}

#[test]
fn queue_verifies_lane_two_on_main_plus_b1_and_lands_both() {
    let repo = QueueRepo::new();
    commit_file(&repo.work_a, "docs/a.md", "branch a\n");
    commit_file(&repo.work_b, "docs/b.md", "branch b\n");
    let head_a = git(&repo.work_a, &["rev-parse", "HEAD"]);
    let head_b = git(&repo.work_b, &["rev-parse", "HEAD"]);
    let (output, report) = repo.run(&[
        "merge",
        "--queue",
        "wt/a/work",
        "wt/b/work",
        "--gate",
        "--no-push",
        "--json",
        "--lanes",
        "2",
        "--target-dir",
        "queue-target",
    ]);
    assert_exit(&output, &report, 0);
    let summary = &report["queue"];
    assert_eq!(summary["lanes"], 2);
    assert_eq!(summary["mainBefore"], repo.base);
    assert_eq!(summary["pushed"], false);
    let branches = summary["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 2);
    let a = &branches[0];
    let b = &branches[1];
    assert_eq!(a["branch"], "wt/a/work");
    assert_eq!(b["branch"], "wt/b/work");
    assert_eq!(a["status"], "landed");
    assert_eq!(b["status"], "landed");
    let merge_a = a["landedSha"].as_str().unwrap().to_owned();
    let merge_b = b["landedSha"].as_str().unwrap().to_owned();
    // b1 verified on the original main; b2 verified speculatively on the
    // exact merge commit b1 landed, and that verification was reused.
    assert_eq!(a["verifications"][0]["base"], repo.base);
    assert_eq!(a["verifications"][0]["lane"], 1);
    assert_eq!(a["verifications"][0]["speculative"], false);
    assert_eq!(b["verifications"][0]["base"], merge_a);
    assert_eq!(b["verifications"][0]["lane"], 2);
    assert_eq!(b["verifications"][0]["speculative"], true);
    assert_eq!(b["verifications"][0]["reused"], true);
    assert_eq!(b["verifications"][0]["merge"], merge_b);
    assert_eq!(summary["mainAfter"], merge_b);
    // Final history: main -> merge_b(merge_a, head_b), merge_a(base, head_a).
    assert_eq!(repo.main(), merge_b);
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", &merge_a]),
        format!("{} {}", repo.base, head_a)
    );
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", &merge_b]),
        format!("{} {}", merge_a, head_b)
    );
    // Lanes used separate Cargo target directories and per-lane e2e ports.
    let targets: Vec<String> = repo
        .trace()
        .iter()
        .map(|event| event["target"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        targets
            .iter()
            .any(|target| target.ends_with("queue-target")),
        "{targets:?}"
    );
    assert!(
        targets
            .iter()
            .any(|target| target.ends_with("queue-target-lane2")),
        "{targets:?}"
    );
    let hub_ports: Vec<String> = repo
        .trace()
        .iter()
        .map(|event| event["hubListen"].as_str().unwrap().to_owned())
        .collect();
    assert!(hub_ports.contains(&"127.0.0.1:58980".to_owned()));
    assert!(hub_ports.contains(&"127.0.0.1:58990".to_owned()));
    // No merge pins linger after every branch has settled.
    let pins = git(&repo.root, &["for-each-ref", "refs/remuda/merge/"]);
    assert!(pins.is_empty(), "leftover merge pins: {pins}");
    repo.assert_cleaned();
}

#[test]
fn queue_catches_a_semantic_conflict_between_independent_branches() {
    let repo = QueueRepo::new();
    // b1 adds a check ("a test"); b2 breaks the state the check requires.
    // Each branch passes alone; the merged main+b1+b2 tree fails the gate.
    commit_file(&repo.work_a, "expect-ok.txt", "state must stay ok\n");
    commit_file(&repo.work_b, "state.txt", "bad\n");
    let (output, report) = repo.run(&[
        "merge",
        "--queue",
        "wt/a/work",
        "wt/b/work",
        "--gate",
        "--no-push",
        "--json",
        "--lanes",
        "2",
    ]);
    assert_exit(&output, &report, 1);
    let branches = report["queue"]["branches"].as_array().unwrap();
    assert_eq!(branches[0]["status"], "landed");
    assert_eq!(branches[1]["status"], "gate_failed");
    let merge_a = branches[0]["landedSha"].as_str().unwrap().to_owned();
    // b2's gate ran on the (main+b1) merge — its base is exactly what b1
    // landed — and failed there.
    let verifications = branches[1]["verifications"].as_array().unwrap();
    assert!(
        verifications.iter().any(|v| {
            v["base"] == merge_a && v["status"] == "failed" && v["speculative"] == true
        })
    );
    assert_eq!(branches[1]["landedSha"], Value::Null);
    // Main advanced only as far as b1; b2's broken merge never lands.
    assert_eq!(repo.main(), merge_a);
    assert_eq!(
        git(&repo.root, &["show", "main:state.txt"]),
        "ok",
        "b2 never lands"
    );
    assert_eq!(
        git(&repo.root, &["cat-file", "-e", "main:expect-ok.txt"]),
        ""
    );
    // Dead speculative merges must not stay pinned after the queue ends.
    assert!(git(&repo.root, &["for-each-ref", "refs/remuda/merge/"]).is_empty());
    repo.assert_cleaned();
}

#[test]
fn queue_reverifies_b2_onto_main_when_b1_fails() {
    let repo = QueueRepo::new();
    // b1 breaks the gate outright; b2 is a docs-only change.
    commit_file(&repo.work_a, "gate-deny.txt", "b1 is broken\n");
    commit_file(&repo.work_b, "docs/b.md", "branch b\n");
    let head_b = git(&repo.work_b, &["rev-parse", "HEAD"]);
    let (output, report) = repo.run(&[
        "merge",
        "--queue",
        "wt/a/work",
        "wt/b/work",
        "--gate",
        "--no-push",
        "--json",
        "--lanes",
        "2",
    ]);
    assert_exit(&output, &report, 1);
    let branches = report["queue"]["branches"].as_array().unwrap();
    assert_eq!(branches[0]["status"], "gate_failed");
    assert_eq!(branches[0]["landedSha"], Value::Null);
    assert_eq!(branches[1]["status"], "landed");
    // b2 first passed speculatively onto b1's tentative merge, but that base
    // never became main (affected selection excluded b1's breakage); after b1
    // failed it was re-verified onto the real main and landed there.
    let verifications = branches[1]["verifications"].as_array().unwrap();
    assert_eq!(verifications.len(), 2, "{verifications:?}");
    assert_eq!(verifications[0]["speculative"], true);
    assert_eq!(verifications[0]["status"], "passed");
    assert_eq!(verifications[0]["reused"], false);
    assert_eq!(verifications[1]["base"], repo.base);
    assert_eq!(verifications[1]["speculative"], false);
    assert_eq!(verifications[1]["status"], "passed");
    assert_eq!(verifications[1]["reused"], true);
    let merge_b = branches[1]["landedSha"].as_str().unwrap().to_owned();
    assert_eq!(repo.main(), merge_b);
    assert_eq!(
        git(&repo.root, &["show", "-s", "--format=%P", &merge_b]),
        format!("{} {}", repo.base, head_b)
    );
    // b1's failed merge pin and b2's speculative pin are both swept.
    assert!(git(&repo.root, &["for-each-ref", "refs/remuda/merge/"]).is_empty());
    repo.assert_cleaned();
}

// ---------------------------------------------------------------------------
// Lane death (2026-09-15 production incident regressions)
// ---------------------------------------------------------------------------

/// Find descendant processes of `root` whose cmdline contains `needle`.
/// Polls briefly: the lane is launched only after the gate reaches the step.
fn descendant_with_cmd(root: u32, needle: &[u8]) -> Option<u32> {
    fn stat_ppid(pid: u32) -> Option<u32> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let comm_end = stat.rfind(')')?;
        stat[comm_end + 2..].split_whitespace().nth(1)?.parse().ok()
    }
    for _ in 0..100 {
        let mut matches = Vec::new();
        let mut frontier = vec![root];
        while let Some(pid) = frontier.pop() {
            let entries = match fs::read_dir("/proc") {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let Ok(other) = name.parse::<u32>() else {
                    continue;
                };
                if stat_ppid(other) == Some(pid) {
                    frontier.push(other);
                    if let Ok(cmdline) = fs::read(format!("/proc/{other}/cmdline"))
                        && cmdline.windows(needle.len()).any(|w| w == needle)
                    {
                        matches.push(other);
                    }
                }
            }
        }
        if let Some(pid) = matches.into_iter().min() {
            return Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

/// Wait until the gate trace contains a step event matching all predicates.
fn wait_for_trace<F>(path: &Path, matches: F)
where
    F: Fn(&Value) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && text
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| matches(&event))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "gate trace never matched"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn queue_killed_lane_is_a_failed_branch_while_others_land() {
    let repo = QueueRepo::new();
    commit_file(
        &repo.work_a,
        "queue-hang.txt",
        "lane a will be killed externally\n",
    );
    commit_file(&repo.work_b, "docs/b.md", "branch b\n");
    // A third branch exercises the "settle before dispatch" ordering fix:
    // it must re-verify onto main after a's death even when a is settled
    // in the same event round.
    let work_c = repo.root.parent().unwrap().join("work-c");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "-b",
            "wt/c/work",
            work_c.to_str().unwrap(),
        ],
    );
    commit_file(&work_c, "docs/c.md", "branch c\n");

    let child = repo
        .command()
        .args([
            "merge",
            "--queue",
            "wt/a/work",
            "wt/b/work",
            "wt/c/work",
            "--gate",
            "--no-push",
            "--json",
            "--lanes",
            "2",
            "--target-dir",
            "kill-queue-target",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Lane a parks inside cargo-test; kill its remuda lane process exactly
    // like the operator did on 2026-09-15. The parent-death guard in the
    // gate driver takes the sleeping gate/stub down with it.
    wait_for_trace(&repo.trace, |event| {
        event["step"] == "cargo-test"
            && event["target"]
                .as_str()
                .is_some_and(|target| target.ends_with("kill-queue-target"))
    });
    let lane = descendant_with_cmd(child.id(), b"merge\x00wt/a/work\x00--gate".as_slice())
        .expect("lane process for wt/a/work");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(lane as i32),
        nix::sys::signal::SIGKILL,
    )
    .unwrap();

    let output = child.wait_with_output().unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("JSON {error}: {}", String::from_utf8_lossy(&output.stderr))
    });
    assert_exit(&output, &report, 1);
    let branches = report["queue"]["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 3);
    assert_eq!(branches[0]["branch"], "wt/a/work");
    assert_eq!(branches[0]["status"], "gate_failed");
    assert_eq!(branches[0]["landedSha"], Value::Null);
    let a_attempts = branches[0]["verifications"].as_array().unwrap();
    assert_eq!(a_attempts.len(), 1);
    assert_eq!(a_attempts[0]["status"], "failed");
    let why = a_attempts[0]["reason"].as_str().unwrap();
    assert!(why.starts_with("lane died: killed by signal"), "{why}");
    assert!(
        branches[0]["why"].as_str().unwrap().contains("lane died"),
        "{}",
        branches[0]["why"]
    );
    assert_eq!(branches[1]["status"], "landed");
    assert_eq!(branches[2]["status"], "landed");
    assert!(git(&repo.root, &["for-each-ref", "refs/remuda/merge/"]).is_empty());
    repo.assert_cleaned();
}

#[test]
fn queue_watchdog_kills_a_hung_lane_and_the_queue_completes() {
    let repo = QueueRepo::new();
    commit_file(&repo.work_a, "queue-hang.txt", "the gate never returns\n");
    let output = {
        let mut command = repo.command();
        command
            .args([
                "merge",
                "--queue",
                "wt/a/work",
                "--gate",
                "--no-push",
                "--json",
                "--lanes",
                "1",
            ])
            // Three seconds of an unreported lane means it is wedged; the
            // watchdog must reap it instead of parking forever.
            .env("REMUDA_QUEUE_WATCHDOG_SECS", "3")
            .env("REMUDA_QUEUE_REAP_GRACE_MS", "1000");
        command.output().unwrap()
    };
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_exit(&output, &report, 1);
    let branches = report["queue"]["branches"].as_array().unwrap();
    assert_eq!(branches[0]["status"], "gate_failed");
    assert_eq!(branches[0]["landedSha"], Value::Null);
    assert!(
        branches[0]["why"].as_str().unwrap().contains("lane died"),
        "{}",
        branches[0]["why"]
    );
    // The sleeping gate subprocess (its own process group) must not outlive
    // the queue: no remuda merge process for this temp repo remains.
    assert_eq!(repo.main(), repo.base, "a hung gate never lands");
    repo.assert_cleaned();
}

// ---------------------------------------------------------------------------
// Concurrency guard
// ---------------------------------------------------------------------------

#[test]
fn concurrent_merge_exits_three_and_wait_runs_after_the_lock_frees() {
    use std::os::unix::process::CommandExt;
    let repo = QueueRepo::new();
    commit_file(
        &repo.work_a,
        "queue-hang.txt",
        "first run holds the locks\n",
    );
    commit_file(&repo.work_b, "docs/b.md", "branch b\n");

    // First run: hangs in the gate while holding the repository lock.
    let mut first = repo
        .command()
        .args(["merge", "wt/a/work", "--gate", "--no-push", "--json"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_trace(&repo.trace, |event| event["step"] == "cargo-test");

    // A second run without --wait exits 3 and names the holder.
    let contended = repo
        .command()
        .args(["merge", "wt/b/work", "--gate", "--no-push", "--json"])
        .output()
        .unwrap();
    let contended_report: Value = serde_json::from_slice(&contended.stdout).unwrap();
    assert_exit(&contended, &contended_report, 3);
    assert_eq!(contended_report["status"], "locked");
    let message = contended_report["error"].as_str().unwrap();
    assert!(
        message.starts_with("another remuda merge is running on this repo (pid "),
        "{message}"
    );
    assert!(message.contains("since "), "{message}");

    // --wait blocks, then proceeds after the holder is gone. b is docs-only,
    // so its gate passes and it lands.
    let waiter = repo
        .command()
        .args([
            "merge",
            "wt/b/work",
            "--gate",
            "--wait",
            "--no-push",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Give the waiter time to block on the lock.
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Releasing the holder (and its hung gate subtree) unblocks the waiter.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(first.id() as i32)),
        nix::sys::signal::SIGKILL,
    )
    .unwrap();
    let _ = first.wait();

    let landed = waiter.wait_with_output().unwrap();
    let landed_report: Value = serde_json::from_slice(&landed.stdout).unwrap_or_else(|error| {
        panic!("JSON {error}: {}", String::from_utf8_lossy(&landed.stderr))
    });
    assert_exit(&landed, &landed_report, 0);
    assert_eq!(landed_report["status"], "ok");
}
