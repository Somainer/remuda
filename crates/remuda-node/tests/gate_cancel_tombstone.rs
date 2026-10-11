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
    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
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
    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
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

/// Records never expire: there is no time dimension anymore, so the same
/// canceled generation is refused on every arrival however delayed, and only
/// a strictly newer generation runs.
#[tokio::test]
async fn a_delayed_old_generation_is_refused_however_late_it_arrives() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
    let repo = dir.path().join("never-checked-out-late");
    let job_id = "gjb_tombstone_late";

    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": job_id, "generation": 1 }),
    )
    .await;
    assert_eq!(
        node.gate_test_canceled_generation(job_id).await,
        Some(1),
        "the record persists with no TTL"
    );

    // The canceled generation and an older one are refused on arrival (each
    // would model an arbitrarily delayed dispatch — the floor no longer
    // consults a clock).
    for generation in [1u32, 0] {
        let result = gate_rpc(
            &node,
            "gate.run",
            run_params(job_id, "lane-late", &repo, generation),
        )
        .await;
        assert_eq!(
            result["status"],
            json!("canceled"),
            "generation {generation} must be refused however late: {result}"
        );
        // Re-refused: the same generation is recorded again, not consumed.
        assert_eq!(
            node.gate_test_canceled_generation(job_id).await,
            Some(1),
            "the canceled generation is not one-shot"
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

/// Test authorizer standing in for the Hub fence.
struct StubAuthorizer(bool);

impl remuda_node::GateAuthorizer for StubAuthorizer {
    fn authorize<'a>(
        &'a self,
        _job_id: &'a str,
        _generation: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(self.0))
    }

    fn request_land_grant<'a>(
        &'a self,
        _job_id: &'a str,
        _generation: u32,
        _merge_sha: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = remuda_node::GateLandGrantAnswer> + Send + 'a>,
    > {
        // Stub grants iff it authorizes.
        Box::pin(std::future::ready(remuda_node::GateLandGrantAnswer {
            granted: self.0,
            landing_in_progress: false,
        }))
    }
}

/// Even with NO local cancel record, the Hub fence refuses a generation the
/// Hub no longer considers live (restart lost the registry, or a record was
/// never recorded): the run answers canceled before touching git.
#[tokio::test]
async fn the_hub_authority_fence_refuses_a_run_the_hub_does_not_authorize() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-authorized");
    let job_id = "gjb_hub_fence_refuse";

    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(false)))
        .await;
    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-fence-refuse", &repo, 1),
    )
    .await;
    assert_eq!(
        result["status"],
        json!("canceled"),
        "a Hub-refused dispatch must not execute: {result}"
    );
    // Nothing executed: the missing checkout was never created.
    assert!(!repo.exists(), "the refused run entered the checkout");
}

/// With the Hub fence AUTHORIZED, a run proceeds (and fails on the missing
/// checkout rather than refusing).
#[tokio::test]
async fn the_hub_authority_fence_allows_an_authorized_run() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-authorized-ok");
    let job_id = "gjb_hub_fence_allow";

    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-fence-allow", &repo, 1),
    )
    .await;
    assert_ne!(
        result["status"],
        json!("canceled"),
        "an authorized dispatch must run: {result}"
    );
}

/// With no authorizer registered (missing/dead link simulation after
/// clearing), the run fails closed: canceled, no git.
#[tokio::test]
async fn a_missing_hub_link_fails_closed() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-no-link");
    let job_id = "gjb_hub_fence_no_link";

    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
    node.gate_test_clear_authorizer().await;
    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-no-link", &repo, 1),
    )
    .await;
    assert_eq!(
        result["status"],
        json!("canceled"),
        "with no Hub link the run must fail closed: {result}"
    );
}

/// (r10 1a) At the job-count cap a NEW job's cancel is REFUSED (no silent
/// eviction of an older job's authority); an already-tracked job still
/// records a higher generation in place.
#[tokio::test]
async fn at_capacity_a_new_cancel_is_refused_not_evicted() {
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    // Fill to capacity with distinct jobs (no run is ever sent).
    for i in 0..4096u32 {
        let job = format!("gjb_cap_{i}");
        gate_rpc(
            &node,
            "gate.cancel",
            json!({ "jobId": job, "generation": 1 }),
        )
        .await;
    }
    // A tracked job updates in place and still succeeds.
    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": "gjb_cap_0", "generation": 2 }),
    )
    .await;
    assert_eq!(
        node.gate_test_canceled_generation("gjb_cap_0").await,
        Some(2)
    );
    // A NEW job over the cap must be refused, not silently evict anyone.
    let overflow = node
        .dispatch_gate_rpc(
            "gate.cancel",
            &json!({ "jobId": "gjb_cap_overflow", "generation": 1 }),
        )
        .await;
    assert!(
        overflow.is_err(),
        "an over-capacity cancel must error: {overflow:?}"
    );
    // The oldest tracked authority is intact (nothing evicted).
    assert_eq!(
        node.gate_test_canceled_generation("gjb_cap_0").await,
        Some(2),
        "cancel authority must never be evicted"
    );
    assert_eq!(
        node.gate_test_canceled_generation("gjb_cap_4095").await,
        Some(1)
    );
}

/// (r10 1b) If the Node loses its local canceled record (process restart),
/// the Hub fence is what stops the run: with an empty local floor but a
/// refusing Hub the run is canceled before git; with an authorizing Hub it
/// proceeds.
#[tokio::test]
async fn after_local_state_is_lost_the_hub_fence_still_refuses() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("never-after-restart");
    let job_id = "gjb_restart_fence";

    // A cancel was recorded before the restart.
    gate_rpc(
        &node,
        "gate.cancel",
        json!({ "jobId": job_id, "generation": 1 }),
    )
    .await;
    assert_eq!(node.gate_test_canceled_generation(job_id).await, Some(1));

    // Simulate the process restart: a fresh registry has no local record,
    // and the carrier attaches fresh (Hub still says this gen is canceled).
    node.gate_test_reset_cancel_records().await;
    assert_eq!(node.gate_test_canceled_generation(job_id).await, None);
    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(false)))
        .await;

    let result = gate_rpc(
        &node,
        "gate.run",
        run_params(job_id, "lane-restart", &repo, 1),
    )
    .await;
    assert_eq!(
        result["status"],
        json!("canceled"),
        "the Hub fence must refuse after local state was lost: {result}"
    );
    assert!(!repo.exists(), "no git after Hub refusal");
}

/// r11 item 7: at capacity, the refused NEW job's cancel is not recorded, so a
/// later delayed dispatch for it is NOT blocked by the local floor (it reaches
/// the Hub-authorize stage; with no carrier it falls through to execution and
/// fails on git — proving nothing was silently evicted/recorded). The tracked
/// full-capacity jobs still refuse their dispatch by the floor and never push.
#[tokio::test]
async fn at_capacity_a_refused_cancel_does_not_block_a_later_dispatch() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    // A live carrier authorizes executions; the tombstone (not the Hub) is
    // what must still refuse the tracked job below.
    node.gate_test_set_authorizer(std::sync::Arc::new(StubAuthorizer(true)))
        .await;
    for i in 0..4096u32 {
        gate_rpc(
            &node,
            "gate.cancel",
            json!({ "jobId": format!("gjb_cap2_{i}"), "generation": 1 }),
        )
        .await;
    }
    // Overflow job's cancel is refused.
    assert!(
        node.dispatch_gate_rpc(
            "gate.cancel",
            &json!({ "jobId": "gjb_cap2_overflow", "generation": 1 }),
        )
        .await
        .is_err()
    );

    // (1) a tracked at-capacity job's dispatch is refused by the floor and
    // never reaches git.
    let tracked_repo = dir.path().join("tracked-no-push");
    let tracked = gate_rpc(
        &node,
        "gate.run",
        run_params("gjb_cap2_0", "lane-cap-tracked", &tracked_repo, 1),
    )
    .await;
    assert_eq!(
        tracked["status"],
        json!("canceled"),
        "the tracked job's dispatch must be refused before git: {tracked}"
    );
    assert!(
        !tracked_repo.exists(),
        "refused dispatch must not create the checkout"
    );

    // (2) the overflow job (no record) is NOT blocked by the local floor:
    // with no carrier attached it executes (and fails on the missing repo —
    // not canceled), proving the refused cancel recorded nothing.
    let overflow_repo = dir.path().join("overflow-runs");
    let overflow = gate_rpc(
        &node,
        "gate.run",
        run_params("gjb_cap2_overflow", "lane-cap-overflow", &overflow_repo, 1),
    )
    .await;
    assert_ne!(
        overflow["status"],
        json!("canceled"),
        "the refused cancel must not block the overflow dispatch: {overflow}"
    );
}

/// r12 item 5: a FRESH production DevNode that has never had a carrier attach
/// has no authorizer at all. A valid land dispatch must be refused (never
/// silently allowed by a "direct/dev" local floor) and so can never push.
#[tokio::test]
async fn a_land_run_with_no_carrier_attached_is_refused_and_pushes_nothing() {
    let dir = tempfile::tempdir().expect("tmp");
    let node = DevNode::new(&DevServerConfig::loopback(0)).expect("node");
    let repo = dir.path().join("fresh-no-carrier");
    let job_id = "gjb_fresh_no_carrier_land";

    // Deliberately do NOT call gate_test_set_authorizer: no carrier attached.
    let mut params = run_params(job_id, "lane-fresh", &repo, 1);
    params["mode"] = json!("land");
    params["push"] = json!(true);

    let result = gate_rpc(&node, "gate.run", params).await;
    assert_eq!(
        result["status"],
        json!("canceled"),
        "missing authority must refuse, not allow a land: {result}"
    );
    // Refused before execute_gate, so no checkout/push was attempted.
    assert!(!repo.exists(), "the refused run must not touch a checkout");
}
