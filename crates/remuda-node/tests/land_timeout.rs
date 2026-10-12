//! r13 item 5 (formerly r12 item 4): a granted LAND child that overruns its
//! budget must have its WHOLE process group terminated — TERM, bounded wait,
//! KILL, reap — synchronously BEFORE the Node reports. A SIGTERM-ignoring
//! git/ssh descendant can no longer keep running/pushing after the verdict.
//! Because the remote could have accepted the ref just before the client hung,
//! the Node reports the outcome as UNCERTAIN (not failed); the Hub keeps the
//! Landing row and reconciles against the remote.
#![cfg(unix)]

use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use remuda_node::{DevNode, DevServerConfig, GateAuthorizer, GateLandGrantAnswer};
use serde_json::json;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Always-authorizes (and grants) the run so it reaches the land child.
struct GrantAll;
impl GateAuthorizer for GrantAll {
    fn authorize<'a>(
        &'a self,
        _job_id: &'a str,
        _generation: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(true))
    }
    fn request_land_grant<'a>(
        &'a self,
        _job_id: &'a str,
        _generation: u32,
        _merge_sha: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = GateLandGrantAnswer> + Send + 'a>> {
        Box::pin(std::future::ready(GateLandGrantAnswer {
            granted: true,
            landing_in_progress: false,
        }))
    }
}

#[tokio::test]
async fn a_timed_out_land_child_is_killed_and_never_writes_its_marker() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("f.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/hang"]);
    std::fs::write(seed.join("f.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/hang"]);

    let lane = dir.path().join("lane");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            lane.to_str().unwrap(),
        ],
    );
    git(&lane, &["config", "user.email", "t@example.com"]);
    git(&lane, &["config", "user.name", "T"]);
    git(&lane, &["fetch", "-q", "origin"]);
    let target = dir.path().join("target");
    std::fs::create_dir_all(&target).unwrap();

    // The land child sleeps well past the budget and would write a marker and
    // echo "landed" afterward. If the kill works, neither ever happens.
    let marker = dir.path().join("land-marker");
    let bin = dir.path().join("fake-land.sh");
    let script = format!(
        r#"#!/bin/bash
set -eu
branch=""; onto="main"; mode=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onto) onto="$2"; shift 2;;
    --land) mode=land; shift;;
    --gate) mode=gate; shift;;
    --no-push) shift;;
    --repo|--target-dir) shift 2;;
    --*) shift;;
    merge) shift;;
    *) if [ -z "$branch" ]; then branch="$1"; fi; shift;;
  esac
done
git fetch -q origin
if [ "$mode" = "gate" ]; then
  git checkout -qB "$onto" "origin/$onto"
  base=$(git rev-parse "$onto"); head=$(git rev-parse "origin/$branch")
  git merge -q --no-ff "origin/$branch" -m "merge $branch"
  printf '{{"exitCode":0,"status":"verified","base":"%s","head":"%s","merged":"%s"}}\n' "$base" "$head" "$(git rev-parse HEAD)"
else
  sleep 3
  echo landed > "{marker}"
  git push -q origin "HEAD:$onto"
  printf '{{"exitCode":0,"status":"landed","merged":"%s"}}\n' "$(git rev-parse HEAD)"
fi
"#,
        marker = marker.display()
    );
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let node = DevNode::new(&DevServerConfig::loopback(0)).unwrap();
    node.gate_test_set_authorizer(std::sync::Arc::new(GrantAll))
        .await;
    node.gate_test_set_land_timeout(Duration::from_secs(1))
        .await;

    let params = json!({
        "jobId": "gjb_land_timeout", "laneId": "lane1",
        "repoPath": lane.to_string_lossy(),
        "targetDir": target.to_string_lossy(),
        "branch": "wt/land/hang", "baseBranch": "main",
        "mode": "land", "web": "never", "push": true, "generation": 1,
        "binary": bin.to_string_lossy(),
    });

    let started = Instant::now();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        node.dispatch_gate_rpc("gate.run", &params),
    )
    .await
    .expect("gate.run hung")
    .expect("gate.run errored");
    // Returns promptly after the 2s budget, not after the 30s sleep.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    // r13 item 5: a timed-out land reports UNCERTAIN (the group is fully
    // terminated first), never a confident "failed" — the ref could already
    // have reached the remote before the client was killed.
    assert_eq!(reply["status"], json!("uncertain"), "{reply}");
    let error = reply["error"].as_str().unwrap_or("");
    assert!(
        error.contains("terminated the land process group"),
        "{error}"
    );

    // The synchronous termination completed before the reply: give no
    // additional grace and prove the killed group never wrote the marker it
    // would have produced after the hang — the push cannot have happened.
    assert!(
        !marker.exists(),
        "the hung land child wrote its marker after the timeout"
    );

    // And origin main is unchanged (still the seeded base, no merge pushed).
    let origin_main = git(&origin, &["rev-parse", "main"]);
    // Seed main never got the merge; origin main must equal that init commit.
    let init_commit = git(&seed, &["rev-parse", "main"]);
    assert_eq!(
        origin_main, init_commit,
        "main advanced despite the killed land"
    );
}

/// r12 item 7: `merge --land` exits NONZERO when the CAS lost (base moved)
/// but still emits its structured verdict. The Node must keep base-moved (and
/// the currentMain sha) so the Hub runs the bounded reverify/requeue, instead
/// of collapsing the nonzero exit to a generic failed.
#[tokio::test]
async fn a_nonzero_land_exit_carrying_base_moved_keeps_the_requeue_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let origin = dir.path().join("origin.git");
    Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("f.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/moved"]);
    std::fs::write(seed.join("f.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/moved"]);

    let lane = dir.path().join("lane");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            lane.to_str().unwrap(),
        ],
    );
    git(&lane, &["config", "user.email", "t@example.com"]);
    git(&lane, &["config", "user.name", "T"]);
    git(&lane, &["fetch", "-q", "origin"]);
    let target = dir.path().join("target");
    std::fs::create_dir_all(&target).unwrap();

    let bin = dir.path().join("fake-land-moved.sh");
    let script = r#"#!/bin/bash
set -eu
branch=""; onto="main"; mode=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onto) onto="$2"; shift 2;;
    --land) mode=land; shift;;
    --gate) mode=gate; shift;;
    --no-push) shift;;
    --repo|--target-dir) shift 2;;
    --*) shift;;
    merge) shift;;
    *) if [ -z "$branch" ]; then branch="$1"; fi; shift;;
  esac
done
git fetch -q origin
if [ "$mode" = "gate" ]; then
  git checkout -qB "$onto" "origin/$onto"
  base=$(git rev-parse "$onto"); head=$(git rev-parse "origin/$branch")
  git merge -q --no-ff "origin/$branch" -m "merge $branch"
  printf '{"exitCode":0,"status":"verified","base":"%s","head":"%s","merged":"%s"}\n' "$base" "$head" "$(git rev-parse HEAD)"
else
  # CAS lost: nonzero exit, but the structured verdict is still on stdout.
  printf '{"exitCode":3,"status":"base_moved","currentMain":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","error":"remote main moved"}\n'
  exit 3
fi
"#;
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let node = DevNode::new(&DevServerConfig::loopback(0)).unwrap();
    node.gate_test_set_authorizer(std::sync::Arc::new(GrantAll))
        .await;

    let params = json!({
        "jobId": "gjb_land_base_moved", "laneId": "lane1",
        "repoPath": lane.to_string_lossy(),
        "targetDir": target.to_string_lossy(),
        "branch": "wt/land/moved", "baseBranch": "main",
        "mode": "land", "web": "never", "push": true, "generation": 1,
        "binary": bin.to_string_lossy(),
    });
    let reply = tokio::time::timeout(
        Duration::from_secs(30),
        node.dispatch_gate_rpc("gate.run", &params),
    )
    .await
    .expect("gate.run hung")
    .expect("gate.run errored");

    assert_eq!(
        reply["status"],
        json!("base-moved"),
        "nonzero land exit with a verdict must keep base-moved: {reply}"
    );
    assert_eq!(
        reply["currentMainSha"],
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "currentMain sha is preserved for the requeue: {reply}"
    );
}

/// r13 item 5: a descendant in the land process group that IGNORES SIGTERM
/// (a wedged ssh that would push/write a marker a moment later) must be
/// SIGKILLed and reaped SYNCHRONOUSLY before the gate verdict returns. The
/// verdict is `uncertain` (never a confident failure), and by the time it is
/// back there is no surviving member of the group and the late marker is never
/// written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sigterm_ignoring_descendant_is_killed_and_reaped_before_the_verdict() {
    let dir = tempfile::tempdir().unwrap();
    // A real bare origin + lane clone so the node's pre-child fetch/ff work.
    let origin = dir.path().join("origin.git");
    Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(&origin)
        .status()
        .unwrap();
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    git(&seed, &["config", "user.email", "t@example.com"]);
    git(&seed, &["config", "user.name", "T"]);
    std::fs::write(seed.join("f.txt"), "base\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["checkout", "-q", "-b", "wt/land/stubborn"]);
    std::fs::write(seed.join("f.txt"), "task\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "task"]);
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main", "wt/land/stubborn"]);
    let lane = dir.path().join("lane");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            lane.to_str().unwrap(),
        ],
    );
    git(&lane, &["config", "user.email", "t@example.com"]);
    git(&lane, &["config", "user.name", "T"]);
    git(&lane, &["fetch", "-q", "origin"]);
    std::fs::create_dir_all(dir.path().join("target")).unwrap();

    let marker = dir.path().join("late-push");
    let pgid_file = dir.path().join("pgid");

    // --gate just succeeds; --land records its pgid, forks a SIGTERM-ignoring
    // child that would write the late marker after 3 s, and blocks until killed.
    let bin = dir.path().join("stubborn-land.sh");
    let script = format!(
        r#"#!/bin/bash
set -eu
branch=""; onto="main"; mode=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onto) onto="$2"; shift 2;;
    --land) mode=land; shift;;
    --gate) mode=gate; shift;;
    --no-push) shift;;
    --repo|--target-dir) shift 2;;
    --*) shift;;
    merge) shift;;
    *) if [ -z "$branch" ]; then branch="$1"; fi; shift;;
  esac
done
if [ "$mode" = "gate" ]; then
  git fetch -q origin
  git checkout -qB "$onto" "origin/$onto"
  git merge -q --no-ff "origin/$branch" -m "merge $branch"
  base=$(git rev-parse "$onto"); head=$(git rev-parse "origin/$branch")
  printf '{{"exitCode":0,"status":"verified","base":"%s","head":"%s","merged":"%s"}}
' "$base" "$head" "$(git rev-parse HEAD)"
else
  echo $$ > "{pgid_file}"
  (
    trap '' TERM
    sleep 3
    echo pushed > "{marker}"
  ) &
  wait
fi
"#,
        marker = marker.display(),
        pgid_file = pgid_file.display()
    );
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let node = DevNode::new(&DevServerConfig::loopback(0)).unwrap();
    node.gate_test_set_authorizer(std::sync::Arc::new(GrantAll))
        .await;
    node.gate_test_set_land_timeout(Duration::from_millis(800))
        .await;

    let params = json!({
        "jobId": "gjb_stubborn_land", "laneId": "lane1",
        "repoPath": lane.to_string_lossy(),
        "targetDir": dir.path().join("target").to_string_lossy(),
        "branch": "wt/land/stubborn", "baseBranch": "main",
        "mode": "land", "web": "never", "push": true, "generation": 1,
        "binary": bin.to_string_lossy(),
    });

    let start = Instant::now();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        node.dispatch_gate_rpc("gate.run", &params),
    )
    .await
    .expect("gate.run hung")
    .expect("gate.run errored");

    // Assert the SYNCHRONOUS kill FIRST (independent of the verdict string):
    // read the pgid the land child recorded and require the group to already be
    // empty — the SIGTERM-ignoring child must have been SIGKILLed (and all
    // members exited) before the verdict returned.
    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("land child recorded its pgid")
        .trim()
        .parse()
        .unwrap();
    assert!(
        remuda_node::gate_test_alive_pids_in_group(pgid).is_empty(),
        "a member of the killed land process group survived past the verdict"
    );

    // The group was fully terminated first; the verdict is uncertain, never a
    // confident failure.
    assert_eq!(reply["status"], json!("uncertain"), "{reply}");
    // Returns promptly after ~budget + a bounded grace, well under the 3 s the
    // stubborn descendant needs to write its late marker.
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "took {:?}",
        start.elapsed()
    );

    // The descendant never reached its post-sleep push; no late marker even
    // after sleeping well past it.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !marker.exists(),
        "SIGTERM-ignoring descendant wrote its late marker after the verdict"
    );
}
