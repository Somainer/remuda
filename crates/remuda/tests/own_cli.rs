//! `remuda own` (and the `task` verbs it hangs off) against the in-process
//! Hub. No fake node, no real models — task ledger and ownership are
//! Hub-only state.

use anyhow::Result;
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

#[test]
fn own_help_lists_claim_release_check_and_list() -> Result<()> {
    let output = Command::new(bin()).args(["own", "--help"]).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["claim", "release", "check", "list"] {
        assert!(stdout.contains(sub), "help must list `{sub}`: {stdout}");
    }
    let output = Command::new(bin()).args(["task", "--help"]).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["add", "list", "show", "split", "set-state"] {
        assert!(stdout.contains(sub), "help must list `{sub}`: {stdout}");
    }
    Ok(())
}

struct Hub {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    base: String,
    token: String,
}

async fn spawn_hub() -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    let token = hub.mint_device_token("own-cli").await?;
    let base = format!("http://{}", hub.addr);
    Ok(Hub {
        _dir: dir,
        hub,
        base,
        token,
    })
}

fn run(args: &[&str], hub: &Hub) -> std::process::Output {
    let mut all = args.to_vec();
    all.extend(["--hub", &hub.base, "--token", &hub.token]);
    Command::new(bin()).args(all).output().expect("run remuda")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_and_own_cli_lifecycle_against_hub() -> Result<()> {
    let hub = spawn_hub().await?;

    // A project to host the ledger slice.
    let output = run(&["project", "create", "--name", "own-cli"], &hub);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let project: Value = serde_json::from_slice(&output.stdout)?;
    let project_id = project["id"].as_str().unwrap().to_string();

    // task add: the intent is the root mandate link.
    let output = run(
        &[
            "task",
            "add",
            "--project",
            &project_id,
            "--title",
            "gate scope",
            "--intent",
            "make own check executable",
            "--owns",
            "crates/remuda-hub/src/tasks.rs",
            "--class",
            "implement",
            "--max-turns",
            "40",
        ],
        &hub,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let task: Value = serde_json::from_slice(&output.stdout)?;
    let task_id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["state"], "pending");
    assert_eq!(
        task["mandate"]["chain"][0]["intent"],
        "make own check executable"
    );
    assert_eq!(task["budget"]["maxTurns"], 40);
    assert_eq!(task["owns"], json!(["crates/remuda-hub/src/tasks.rs"]));

    // task list / show.
    let output = run(&["task", "list", "--project", &project_id], &hub);
    assert!(output.status.success());
    let list: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == task_id)
    );
    let output = run(&["task", "show", &task_id], &hub);
    assert!(output.status.success());
    let shown: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(shown["id"], task_id);

    // task split: the child inherits the owner's words.
    let output = run(
        &[
            "task",
            "split",
            &task_id,
            "--title",
            "cli",
            "--intent",
            "wire remuda own check",
            "--owns",
            "crates/remuda/src/cmd/own.rs",
        ],
        &hub,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let child: Value = serde_json::from_slice(&output.stdout)?;
    let child_id = child["id"].as_str().unwrap().to_string();
    assert_eq!(child["parentTaskId"], task_id);
    let chain = child["mandate"]["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0]["intent"], "make own check executable", "§2.5");
    assert_eq!(chain[1]["intent"], "wire remuda own check");

    // set-state drives the legal lifecycle; an illegal jump exits non-zero.
    let output = run(&["task", "set-state", &task_id, "--state", "placed"], &hub);
    assert!(output.status.success());
    let output = run(&["task", "set-state", &task_id, "--state", "running"], &hub);
    assert!(output.status.success());
    let bad = run(
        &["task", "set-state", &child_id, "--state", "running"],
        &hub,
    );
    assert!(!bad.status.success(), "pending → running is illegal");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(stderr.contains("illegal task transition"), "{stderr}");

    // own claim: the child's file is free; the parent's file conflicts.
    let output = run(
        &[
            "own",
            "claim",
            &child_id,
            "--path",
            "crates/remuda/src/cmd/task.rs",
        ],
        &hub,
    );
    assert!(output.status.success());
    let claim: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        claim["owns"],
        json!([
            "crates/remuda/src/cmd/own.rs",
            "crates/remuda/src/cmd/task.rs"
        ])
    );
    let conflict = run(
        &[
            "own",
            "claim",
            &child_id,
            "--path",
            "crates/remuda-hub/src/",
        ],
        &hub,
    );
    assert!(!conflict.status.success());
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(stderr.contains("conflict"), "{stderr}");

    // own list shows both active claims.
    let output = run(&["own", "list", "--project", &project_id], &hub);
    assert!(output.status.success());
    let map: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(map["items"].as_array().unwrap().len(), 2);

    // own check: in-scope paths exit 0; a path crossing the boundary exits 2.
    let output = run(
        &[
            "own",
            "check",
            &task_id,
            "--path",
            "crates/remuda-hub/src/tasks.rs",
        ],
        &hub,
    );
    assert_eq!(output.status.code(), Some(0));
    let check: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check["within"], true);

    let output = run(
        &[
            "own",
            "check",
            &task_id,
            "--path",
            "crates/remuda/src/cmd/merge.rs",
        ],
        &hub,
    );
    assert_eq!(output.status.code(), Some(2), "scope drift maps to exit 2");
    let check: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check["within"], false);
    assert_eq!(
        check["violations"],
        json!(["crates/remuda/src/cmd/merge.rs"])
    );

    // own check over stdin diff (the gate shape: git diff | remuda own check … -).
    let diff = "\
diff --git a/crates/remuda-hub/src/tasks.rs b/crates/remuda-hub/src/tasks.rs
--- a/crates/remuda-hub/src/tasks.rs
+++ b/crates/remuda-hub/src/tasks.rs
@@ -1 +1 @@
-old
+new
";
    let mut child = Command::new(bin())
        .args([
            "own",
            "check",
            &task_id,
            "--diff-file",
            "-",
            "--hub",
            &hub.base,
            "--token",
            &hub.token,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(diff.as_bytes())?;
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(0));
    let check: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(check["checked"], 1);
    assert_eq!(check["within"], true);

    // own release then land; the claim disappears from the map.
    let output = run(
        &[
            "own",
            "release",
            &child_id,
            "--path",
            "crates/remuda/src/cmd/task.rs",
        ],
        &hub,
    );
    assert!(output.status.success());
    let released: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(released["owns"], json!(["crates/remuda/src/cmd/own.rs"]));

    hub.hub.shutdown().await;
    Ok(())
}
