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
fn run_params(job_id: &str, lane_id: &str, repo: &std::path::Path, generation: u32) -> Value {
    json!({
        "jobId": job_id,
        "generation": generation,
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
    let cancel = gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": job_id, "generation": 1 }),
    )
    .await;
    assert_eq!(cancel["ok"], json!(true), "{cancel}");
    assert_eq!(cancel["generation"], json!(1), "{cancel}");

    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-cancel-first", &repo, 1),
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

    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": "gjb_tombstone_a", "generation": 1 }),
    )
    .await;

    // A different job on the same lane runs for real: with no checkout the
    // first git fetch fails, so the verdict is a run failure — anything but
    // the tombstone's canceled.
    let other = gate_rpc(
        &node,
        "gate.run",
        run_params("gjb_other_job", "lane-shared", &repo, 1),
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

    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": job_id, "generation": 1 }),
    )
    .await;
    let first = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-consumed", &repo, 1),
    )
    .await;
    assert_eq!(first["status"], json!("canceled"), "{first}");

    // A strictly NEWER generation (a requeued job re-dispatched) supersedes
    // the cancel: it is no longer suppressed and executes — it fails on the
    // missing checkout instead of answering canceled.
    let second = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-consumed", &repo, 2),
    )
    .await;
    assert_ne!(
        second["status"],
        json!("canceled"),
        "a higher-generation dispatch must supersede the cancel: {second}"
    );
    // The superseding run cleared the record.
    assert_eq!(
        node.gate_test_canceled_generation(job_id).await,
        None,
        "the newer generation must clear the canceled record"
    );
}

/// Elapsed time must not authorize an old dispatch: a generation-1 run that
/// arrives long after the cancel (well beyond the removed 60 s TTL — the
/// clock is warped, not slept) is still refused, an older generation is
/// refused too, and only a strictly newer generation runs.
#[tokio::test]
async fn a_delayed_old_generation_is_refused_however_late_it_arrives() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-checked-out-late");
    let job_id = "gjb_tombstone_late";

    node.gate_test_warp_clock(std::time::Duration::from_secs(60 * 60));
    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": job_id, "generation": 1 }),
    )
    .await;
    // Warp AGAIN between cancel and run: total elapsed far past any old TTL.
    node.gate_test_warp_clock(std::time::Duration::from_secs(60 * 60));
    assert_eq!(
        node.gate_test_canceled_generation(job_id).await,
        Some(1),
        "the record must survive arbitrary elapsed time"
    );

    // An older generation and the canceled generation itself are refused.
    for generation in [0u32, 1] {
        let result = gate_rpc(
            &node,
            "gate.run",
            run_params(job_id, "lane-late", &repo, generation),
        )
        .await;
        assert_eq!(
            result["status"],
            json!("canceled"),
            "generation {generation} must be refused after 2 h: {result}"
        );
    }

    // The strictly newer generation runs (and fails on the missing repo).
    let fresh = gate_rpc(&node, "gate.run", run_params(job_id, "lane-late", &repo, 2)).await;
    assert_ne!(
        fresh["status"],
        json!("canceled"),
        "only a newer generation may run: {fresh}"
    );
}
