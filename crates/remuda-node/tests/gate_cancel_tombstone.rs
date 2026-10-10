//! ma-initiator r6 item 2: a `gate.cancel` that overtakes its `gate.run`
//! must not be acknowledged and then ignored. When the cancel reaches the
//! Node before the run registered, the Node records a short-lived tombstone
//! and the in-flight `gate.run` answers `canceled` WITHOUT executing —
//! otherwise the whole job runs while the Hub already settled it canceled
//! (and a pushFrom-lane land can move main after the cancel was acked).
//!
//! These tests drive the real `gate.run` / `gate.cancel` RPC handlers on a
//! real [`DevNode`] (the same entry point the carriers use), not a mock.

use remuda_node::{DevNode, DevServerConfig};
use serde_json::{Value, json};

/// Params for a gate run pointed at a nonexistent checkout. A run that
/// actually executes fails fast at the first `git fetch` with status
/// `failed`; a tombstoned run must instead return `canceled` before any
/// process is spawned.
fn run_params(job_id: &str, lane_id: &str, repo: &std::path::Path) -> Value {
    json!({
        "jobId": job_id,
        "laneId": lane_id,
        "repoPath": repo,
        "targetDir": repo.join("target"),
        "branch": "branch-under-test",
        "baseBranch": "main",
        "mode": "verify",
        "web": "never",
        "env": {},
        "timeouts": {},
        "gateTimeoutSecs": 0,
        "push": false,
        "keepLogs": false,
    })
}

async fn gate_rpc(node: &DevNode, method: &'static str, params: Value) -> Value {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        node.dispatch_gate_rpc(method, &params),
    )
    .await
    .expect("gate RPC hung")
    .unwrap_or_else(|error| panic!("{method} failed: {error}"))
}

#[tokio::test]
async fn gate_run_is_refused_canceled_when_its_cancel_was_acked_first() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-checked-out");
    let job_id = "gjb_tombstone_cancel_first";

    // The exact ordering the race produces: gate.cancel is processed while
    // no run is registered (dispatch is still between the claim commit and
    // the gate.run send), acked, and only then does gate.run arrive.
    let cancel = gate_rpc(&node, "gate.cancel", json!({ "jobId": job_id })).await;
    assert_eq!(cancel["ok"], json!(true), "{cancel}");

    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-cancel-first", &repo),
    )
    .await;
    assert_eq!(
        result["status"],
        json!("canceled"),
        "the overtaken run must answer canceled, not execute: {result}"
    );
    assert_eq!(result["jobId"], json!(job_id));
}

#[tokio::test]
async fn the_cancel_tombstone_does_not_touch_other_jobs() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-checked-out");

    gate_rpc(&node, "gate.cancel", json!({ "jobId": "gjb_tombstone_a" })).await;

    // A different job on the same lane runs for real: with no checkout the
    // first git fetch fails, so the verdict is a run failure — anything but
    // the tombstone's canceled.
    let other = gate_rpc(
        &node,
        "gate.run",
        run_params("gjb_other_job", "lane-shared", &repo),
    )
    .await;
    assert_ne!(
        other["status"],
        json!("canceled"),
        "the tombstone must be keyed per job id: {other}"
    );
}

#[tokio::test]
async fn the_tombstone_is_consumed_by_the_first_run() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-checked-out");
    let job_id = "gjb_tombstone_consumed";

    gate_rpc(&node, "gate.cancel", json!({ "jobId": job_id })).await;
    let first = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-consumed", &repo),
    )
    .await;
    assert_eq!(first["status"], json!("canceled"), "{first}");

    // One-shot: a later same-jobId run (a fresh Hub dispatch) is no longer
    // suppressed — it executes and fails on the missing checkout instead.
    let second = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-consumed", &repo),
    )
    .await;
    assert_ne!(
        second["status"],
        json!("canceled"),
        "the tombstone must not survive the run it was racing with: {second}"
    );
}
