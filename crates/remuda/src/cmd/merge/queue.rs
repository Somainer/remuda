//! Optimistic merge queue: parallel verification, serial compare-and-swap.
//!
//! Lane 1 verifies branch 1 onto `main`; lane 2 speculatively verifies
//! branch 2 onto branch 1's not-yet-landed merge commit (published as a
//! `.preparing.json` sidecar the moment the merge is constructed). When the
//! predecessor lands, a speculative verification whose base *is* the new main
//! lands directly; otherwise the branch is re-verified onto the real main.
//! Cargo steps run fully parallel (separate `CARGO_TARGET_DIR`s); only the
//! shared-browser web e2e step serialises on an advisory `flock(1)` lock.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{MergeArgs, MergeReport, branch_ref, git, lane_target_dir, reports, resolve};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueSummary {
    pub(super) lanes: usize,
    pub(super) main_before: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) main_after: Option<String>,
    pub(super) pushed: bool,
    pub(super) branches: Vec<BranchOutcome>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchOutcome {
    pub(super) order: usize,
    pub(super) branch: String,
    pub(super) verifications: Vec<VerificationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) landed_sha: Option<String>,
    /// landed | gate_failed | base_moved | skipped
    pub(super) status: String,
    pub(super) why: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VerificationRecord {
    pub(super) lane: usize,
    pub(super) base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) merge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tree: Option<String>,
    /// passed | failed
    pub(super) status: String,
    /// A speculative base is a predecessor merge commit that was not yet main.
    pub(super) speculative: bool,
    /// true when this verification was the one used for landing.
    pub(super) reused: bool,
}

/// A gate verdict injected into the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FinishedJob {
    idx: usize,
    lane: usize,
    base: String,
    merge: Option<String>,
    tree: Option<String>,
    verdict: Verdict,
}

/// What the coordinator should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decision {
    /// Start (or re-start) verification of branch `idx` on `base`.
    Verify {
        idx: usize,
        lane: usize,
        base: String,
        speculative: bool,
        attempt: usize,
    },
    /// Fast-forward main to the already-verified merge commit.
    Land {
        idx: usize,
        base: String,
        merge: String,
    },
    /// Main moved under the queue too many times; report exit code 2.
    StopBaseMoved {
        idx: usize,
        current_main: String,
    },
    Wait,
    Done,
}

/// Pure queue state machine; all git and subprocesses live in the driver.
pub(super) struct Machine {
    main: String,
    status: Vec<BranchStatus>,
    /// Merge commit published by the in-flight/preparing verification, if any.
    tentative: Vec<Option<String>>,
    attempts: Vec<Vec<Attempt>>,
    in_flight: BTreeSet<usize>,
    /// Speculative-launch flag of the running verification, if any.
    launched_speculative: Vec<bool>,
    /// How often a third-party main move forced a re-verification per branch.
    moves: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchStatus {
    Pending,
    Landed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    lane: usize,
    base: String,
    merge: Option<String>,
    tree: Option<String>,
    verdict: Verdict,
    speculative: bool,
}

const MAX_REVERIFIES: u32 = 3;

impl Machine {
    fn new(main: impl Into<String>, branches: usize) -> Self {
        let main = main.into();
        Self {
            main,
            status: vec![BranchStatus::Pending; branches],
            tentative: vec![None; branches],
            attempts: vec![Vec::new(); branches],
            in_flight: BTreeSet::new(),
            launched_speculative: vec![false; branches],
            moves: vec![0; branches],
        }
    }

    fn prepared(&mut self, idx: usize, merge: String) {
        self.tentative[idx] = Some(merge);
    }

    fn finished(&mut self, job: FinishedJob) {
        self.in_flight.remove(&job.idx);
        let speculative = self.launched_speculative[job.idx];
        self.launched_speculative[job.idx] = false;
        self.attempts[job.idx].push(Attempt {
            lane: job.lane,
            base: job.base,
            merge: job.merge,
            tree: job.tree,
            verdict: job.verdict,
            speculative,
        });
        if job.verdict == Verdict::Failed {
            // The constructed tentative merge is dead.
            self.tentative[job.idx] = None;
        }
    }

    fn landed(&mut self, idx: usize, merge: String) {
        self.main = merge;
        self.status[idx] = BranchStatus::Landed;
        self.tentative[idx] = None;
    }

    /// Observe main moving under the queue; the current attempt is stale.
    fn external_move(&mut self, idx: usize, current_main: String) {
        self.main = current_main;
        self.moves[idx] += 1;
    }

    /// Pick verification work that can start now and fits free lanes.
    fn dispatch(&mut self, lanes: usize, busy: &BTreeMap<usize, usize>) -> Vec<Decision> {
        let mut used: BTreeSet<usize> = busy.values().copied().collect();
        let mut decisions = Vec::new();
        for idx in 0..self.status.len() {
            if used.len() >= lanes {
                break;
            }
            if self.status[idx] != BranchStatus::Pending || self.in_flight.contains(&idx) {
                continue;
            }
            // A finished attempt on the current main is next_serial's job
            // (land, or terminal failure). A dead-base attempt reverifies.
            if let Some(last) = self.attempts[idx].last() {
                if last.base == self.main {
                    continue;
                }
                if self.moves[idx] >= MAX_REVERIFIES {
                    continue; // next_serial reports BaseMoved
                }
                if idx > 0 && self.status[idx - 1] == BranchStatus::Pending {
                    continue; // wait for the predecessor to settle
                }
            }
            let (base, speculative) = if idx == 0 || self.attempts[idx].last().is_some() {
                // Head of queue, or a re-verification: the current main.
                (self.main.clone(), false)
            } else {
                let pred = idx - 1;
                match self.status[pred] {
                    BranchStatus::Landed | BranchStatus::Failed => (self.main.clone(), false),
                    BranchStatus::Pending => {
                        // Speculate onto the predecessor's constructed merge.
                        let Some(tentative) = self.tentative[pred].clone() else {
                            continue;
                        };
                        (tentative, true)
                    }
                }
            };
            let lane = (1..=lanes).find(|lane| !used.contains(lane)).unwrap();
            used.insert(lane);
            self.in_flight.insert(idx);
            self.launched_speculative[idx] = speculative;
            decisions.push(Decision::Verify {
                idx,
                lane,
                base,
                speculative,
                attempt: self.attempts[idx].len(),
            });
        }
        decisions
    }

    /// The one serial action to take after current events settle.
    fn next_serial(&mut self) -> Decision {
        let mut waiting = false;
        for idx in 0..self.status.len() {
            if self.status[idx] != BranchStatus::Pending || self.in_flight.contains(&idx) {
                continue;
            }
            let Some(last) = self.attempts[idx].last() else {
                waiting = true; // not dispatched yet
                continue;
            };
            if last.verdict == Verdict::Failed {
                if last.base == self.main {
                    self.status[idx] = BranchStatus::Failed;
                    continue;
                }
                // Dead-base attempt: dispatch() re-verifies once the
                // predecessor has settled.
                waiting = true;
                continue;
            }
            // Passing branches land in queue order.
            if idx > 0 && self.status[idx - 1] == BranchStatus::Pending {
                waiting = true;
                continue;
            }
            let Some(merge) = last.merge.clone() else {
                continue;
            };
            if last.base == self.main {
                return Decision::Land {
                    idx,
                    base: last.base.clone(),
                    merge,
                };
            }
            // The verified base never became main. Stop only after the
            // external-move re-verification budget is exhausted; otherwise
            // dispatch() re-verifies on the current main.
            if self.moves[idx] >= MAX_REVERIFIES
                && !self.attempts[idx].iter().any(|a| a.base == self.main)
            {
                return Decision::StopBaseMoved {
                    idx,
                    current_main: self.main.clone(),
                };
            }
            waiting = true;
        }
        if waiting || !self.in_flight.is_empty() {
            Decision::Wait
        } else if self
            .status
            .iter()
            .all(|status| *status != BranchStatus::Pending)
        {
            Decision::Done
        } else {
            Decision::Wait
        }
    }

    fn records(&self, names: &[String]) -> Vec<BranchOutcome> {
        names
            .iter()
            .enumerate()
            .map(|(idx, name)| {
                let used_merge = if self.status[idx] == BranchStatus::Landed {
                    self.attempts[idx]
                        .iter()
                        .rev()
                        .find(|a| a.verdict == Verdict::Passed)
                        .and_then(|a| a.merge.clone())
                } else {
                    None
                };
                let verifications = self.attempts[idx]
                    .iter()
                    .map(|a| VerificationRecord {
                        lane: a.lane,
                        base: a.base.clone(),
                        merge: a.merge.clone(),
                        tree: a.tree.clone(),
                        status: if a.verdict == Verdict::Passed {
                            "passed".into()
                        } else {
                            "failed".into()
                        },
                        speculative: a.speculative,
                        reused: self.status[idx] == BranchStatus::Landed
                            && a.verdict == Verdict::Passed
                            && a.merge == used_merge,
                    })
                    .collect();
                let (status, why) = match self.status[idx] {
                    BranchStatus::Landed => {
                        ("landed".to_string(), landed_reason(&self.attempts[idx]))
                    }
                    BranchStatus::Failed => (
                        "gate_failed".to_string(),
                        "gate failed on the current main; branch not landed".into(),
                    ),
                    BranchStatus::Pending => (
                        "skipped".to_string(),
                        "queue stopped before this branch".into(),
                    ),
                };
                BranchOutcome {
                    order: idx,
                    branch: name.clone(),
                    verifications,
                    landed_sha: used_merge,
                    status,
                    why,
                }
            })
            .collect()
    }
}

fn landed_reason(attempts: &[Attempt]) -> String {
    let used = attempts
        .iter()
        .rev()
        .find(|a| a.verdict == Verdict::Passed)
        .expect("landed branch has a passing attempt");
    if attempts.len() == 1 {
        if used.speculative {
            "speculative verification reused after predecessor landed".into()
        } else {
            "verified on main and landed".into()
        }
    } else if used.speculative {
        "speculative verification reused after predecessor landed".into()
    } else if attempts.iter().any(|a| a.verdict == Verdict::Failed) {
        "speculative attempt failed; re-verified on main and landed".into()
    } else {
        "re-verified on main after main moved; landed".into()
    }
}

// ---------------------------------------------------------------------------
// Subprocess driver
// ---------------------------------------------------------------------------

enum Event {
    Prepared { idx: usize, merge: String },
    Finished(FinishedJob),
}

pub(crate) fn run_queue(args: MergeArgs) -> MergeReport {
    let mut report = super::new_report(&args.queue.join(", "), &args);
    report.branch = format!("queue[{}]", args.queue.join(", "));
    let result = drive(&args, &mut report);
    if let Err(error) = result {
        report.exit_code = 1;
        report.status = "queue_failed".into();
        report.error = Some(format!("{error:#}"));
    } else if report.exit_code == 0 {
        report.status = "queue_ok".into();
    }
    report
}

struct QueueCtx {
    repo: PathBuf,
    references: Vec<String>,
    lock: PathBuf,
}

fn drive(args: &MergeArgs, report: &mut MergeReport) -> Result<()> {
    ensure!(args.gate, "--queue requires --gate");
    ensure!(args.lanes >= 1, "--lanes must be at least 1");
    let cwd = args.repo.clone().unwrap_or(std::env::current_dir()?);
    let repo = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?);
    git(&repo, &["fetch", "origin"])?;
    let mut names = Vec::new();
    let mut references = Vec::new();
    for branch in &args.queue {
        let reference = branch_ref(branch)?;
        git(&repo, &["check-ref-format", &reference])?;
        ensure!(
            !names.contains(branch),
            "duplicate branch in --queue: {branch}"
        );
        names.push(branch.clone());
        references.push(reference);
    }
    let main_before = resolve(&repo, "refs/heads/main")?;
    let lock = args
        .e2e_lock
        .clone()
        .unwrap_or_else(|| reports::common_dir(&repo).unwrap().join("remuda/e2e.lock"));
    if let Some(parent) = lock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let ctx = QueueCtx {
        repo: repo.clone(),
        references,
        lock,
    };

    let mut machine = Machine::new(main_before.clone(), names.len());
    let (tx, rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    // Busy lane per branch: branch idx -> lane.
    let mut busy: BTreeMap<usize, usize> = BTreeMap::new();
    let lanes = args.lanes;
    let mut main_after = None;
    let mut pushed = false;

    loop {
        for decision in machine.dispatch(lanes, &busy) {
            if let Decision::Verify {
                idx, lane, base, ..
            } = decision
            {
                busy.insert(idx, lane);
                spawn_verify(args, &ctx, tx.clone(), idx, lane, base);
            }
        }

        match machine.next_serial() {
            Decision::Done => {
                main_after = Some(resolve(&ctx.repo, "refs/heads/main")?);
                break;
            }
            Decision::StopBaseMoved { idx, current_main } => {
                let mut outcomes = machine.records(&names);
                outcomes[idx].status = "base_moved".into();
                outcomes[idx].why = format!(
                    "main moved to {current_main} and stayed unverifiable after re-verification"
                );
                for outcome in outcomes.iter_mut().skip(idx + 1) {
                    if outcome.status == "landed" {
                        continue;
                    }
                    outcome.status = "skipped".into();
                    outcome.why = "queue stopped on an unresolved base move".into();
                }
                finish_report(
                    report,
                    lanes,
                    main_before.clone(),
                    Some(current_main),
                    false,
                    outcomes,
                    2,
                );
                return Ok(());
            }
            Decision::Land { idx, base, merge } => {
                let outcome = land_child(&ctx, idx, &base, &merge)?;
                busy.remove(&idx);
                match outcome {
                    LandOutcome::Landed => {
                        machine.landed(idx, merge.clone());
                    }
                    LandOutcome::BaseMoved(current) => {
                        // Stale attempt: force re-verification onto new main.
                        machine.external_move(idx, current);
                    }
                }
                continue;
            }
            Decision::Verify { .. } | Decision::Wait => match rx.recv() {
                Ok(Event::Prepared { idx, merge }) => machine.prepared(idx, merge),
                Ok(Event::Finished(job)) => {
                    busy.remove(&job.idx);
                    machine.finished(job);
                }
                Err(_) => break,
            },
        }
    }

    // Landed branches settle in order; failed branches remain unlanded.
    let outcomes = machine.records(&names);
    let failed = outcomes
        .iter()
        .any(|outcome| outcome.status == "gate_failed");
    let mut push_error = None;
    if !failed && !args.no_push {
        if let Err(error) = git(
            &ctx.repo,
            &[
                "push",
                "origin",
                &format!("{}:refs/heads/main", main_after.clone().unwrap()),
            ],
        ) {
            // Local landings stand, exactly like the single-branch push step.
            push_error = Some(format!("{error:#}"));
        } else {
            pushed = true;
        }
    }
    let exit = if failed || push_error.is_some() { 1 } else { 0 };
    finish_report(
        report,
        lanes,
        main_before,
        main_after,
        pushed,
        outcomes,
        exit,
    );
    if let Some(error) = push_error {
        report.status = "queue_push_failed".into();
        report.error = Some(error);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_report(
    report: &mut MergeReport,
    lanes: usize,
    main_before: String,
    main_after: Option<String>,
    pushed: bool,
    branches: Vec<BranchOutcome>,
    exit_code: i32,
) {
    report.exit_code = exit_code;
    report.main_updated = main_after.as_ref() != Some(&main_before);
    report.pushed = pushed;
    report.expected_main = Some(main_before.clone());
    report.base = Some(main_before.clone());
    report.status = if exit_code == 0 {
        "queue_ok".into()
    } else if exit_code == 1 {
        "queue_gate_failed".into()
    } else {
        "queue_base_moved".into()
    };
    report.queue = Some(QueueSummary {
        lanes,
        main_before,
        main_after,
        pushed,
        branches,
    });
}

enum LandOutcome {
    Landed,
    BaseMoved(String),
}

/// Binary queue children re-spawn. Tests override it (the test harness is not
/// the `remuda` CLI) via REMUDA_MERGE_BIN.
fn merge_binary() -> Result<PathBuf> {
    match std::env::var_os("REMUDA_MERGE_BIN") {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(std::env::current_exe()?),
    }
}

/// Parse the first JSON value in a child's stdout (the CLI pretty-prints).
fn first_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes).into_iter();
    deserializer.next()?.ok()
}

fn land_child(ctx: &QueueCtx, idx: usize, base: &str, merge: &str) -> Result<LandOutcome> {
    let _ = merge; // the child looks the merge up in its persisted report
    let repo_str = ctx.repo.to_string_lossy().into_owned();
    let exe = merge_binary()?;
    let output = Command::new(exe)
        .current_dir(&ctx.repo)
        .arg("merge")
        .arg(ctx.references[idx].trim_start_matches("refs/heads/"))
        .arg("--land")
        .arg("--onto")
        .arg(base)
        .arg("--no-push")
        .arg("--json")
        .arg("--repo")
        .arg(&repo_str)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    let value = first_json(&output.stdout).context("land child produced no JSON report")?;
    match value["exitCode"].as_i64() {
        Some(0) => Ok(LandOutcome::Landed),
        Some(2) => Ok(LandOutcome::BaseMoved(
            value["currentMain"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_default(),
        )),
        _ => Err(anyhow::anyhow!(
            "land failed for {}: {}",
            ctx.references[idx],
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_verify(
    args: &MergeArgs,
    ctx: &QueueCtx,
    tx: Sender<Event>,
    idx: usize,
    lane: usize,
    base: String,
) {
    let repo = ctx.repo.clone();
    let reference = ctx.references[idx].clone();
    let lock = ctx.lock.clone();
    let target = lane_target_dir(&repo, args.target_dir.as_deref(), lane);
    let repo_str = repo.to_string_lossy().into_owned();
    let target_str = target.to_string_lossy().into_owned();
    let lock_str = lock.to_string_lossy().into_owned();
    let exe = merge_binary().unwrap();
    let mut command = Command::new(&exe);
    command
        .current_dir(&repo)
        .arg("merge")
        .arg(reference.trim_start_matches("refs/heads/"))
        .arg("--gate")
        .arg("--onto")
        .arg(&base)
        .arg("--no-push")
        .arg("--json")
        .arg("--repo")
        .arg(&repo_str)
        .arg("--target-dir")
        .arg(&target_str)
        .arg("--e2e-lane")
        .arg(lane.to_string())
        .arg("--e2e-port-base")
        .arg(args.e2e_port_base.to_string())
        .arg("--e2e-lock")
        .arg(&lock_str);
    if args.web {
        command.arg("--web");
    }
    if args.web_e2e {
        command.arg("--web-e2e");
    }
    if args.full {
        command.arg("--full");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    thread::spawn(move || {
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = tx.send(Event::Finished(failed_job(
                    idx,
                    lane,
                    &base,
                    &error.to_string(),
                )));
                return;
            }
        };
        // Watch for the preparing sidecar so the next lane can speculate.
        let mut announced = false;
        loop {
            if !announced && let Ok(Some(preparing)) = reports::preparing(&repo, &reference, &base)
            {
                announced = true;
                let _ = tx.send(Event::Prepared {
                    idx,
                    merge: preparing.merged,
                });
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(std::time::Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                let _ = tx.send(Event::Finished(failed_job(
                    idx,
                    lane,
                    &base,
                    &error.to_string(),
                )));
                return;
            }
        };
        let value: serde_json::Value = match first_json(&output.stdout) {
            Some(value) => value,
            None => {
                let _ = tx.send(Event::Finished(failed_job(
                    idx,
                    lane,
                    &base,
                    "child produced no JSON report",
                )));
                return;
            }
        };
        let verdict = if value["exitCode"] == 0 {
            Verdict::Passed
        } else {
            Verdict::Failed
        };
        let _ = tx.send(Event::Finished(FinishedJob {
            idx,
            lane,
            base,
            merge: value["merged"].as_str().map(str::to_owned),
            tree: value["tree"].as_str().map(str::to_owned),
            verdict,
        }));
    });
}

fn failed_job(idx: usize, lane: usize, base: &str, error: &str) -> FinishedJob {
    eprintln!("queue: verification failed to start: {error}");
    FinishedJob {
        idx,
        lane,
        base: base.to_owned(),
        merge: None,
        tree: None,
        verdict: Verdict::Failed,
    }
}

// ---------------------------------------------------------------------------
// Pure state-machine tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(
        machine: &mut Machine,
        idx: usize,
        lane: usize,
        base: &str,
        merge: Option<&str>,
        verdict: Verdict,
    ) {
        machine.finished(FinishedJob {
            idx,
            lane,
            base: base.into(),
            merge: merge.map(str::to_owned),
            tree: merge.map(|_| "tree".into()),
            verdict,
        });
    }

    fn busy_of(decisions: &[Decision]) -> BTreeMap<usize, usize> {
        decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Verify { idx, lane, .. } => Some((*idx, *lane)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pass_pass_speculates_and_both_land_without_reverify() {
        let mut m = Machine::new("main0", 2);
        let d1 = m.dispatch(2, &BTreeMap::new());
        assert_eq!(
            d1,
            vec![Decision::Verify {
                idx: 0,
                lane: 1,
                base: "main0".into(),
                speculative: false,
                attempt: 0,
            }]
        );
        let mut busy = busy_of(&d1);
        // b1 constructs its merge: b2 can speculate onto it.
        m.prepared(0, "M1".into());
        let d2 = m.dispatch(2, &busy);
        assert_eq!(
            d2,
            vec![Decision::Verify {
                idx: 1,
                lane: 2,
                base: "M1".into(),
                speculative: true,
                attempt: 0,
            }]
        );
        busy.extend(busy_of(&d2));
        assert_eq!(m.next_serial(), Decision::Wait);
        finish(&mut m, 0, 1, "main0", Some("M1"), Verdict::Passed);
        busy.remove(&0);
        assert_eq!(
            m.next_serial(),
            Decision::Land {
                idx: 0,
                base: "main0".into(),
                merge: "M1".into(),
            }
        );
        m.landed(0, "M1".into());
        // b2 still in flight on base M1, which is now main.
        assert_eq!(m.next_serial(), Decision::Wait);
        finish(&mut m, 1, 2, "M1", Some("M2"), Verdict::Passed);
        busy.clear();
        assert_eq!(
            m.next_serial(),
            Decision::Land {
                idx: 1,
                base: "M1".into(),
                merge: "M2".into(),
            }
        );
        m.landed(1, "M2".into());
        assert_eq!(m.next_serial(), Decision::Done);
        let records = m.records(&["wt/a".into(), "wt/b".into()]);
        assert!(records.iter().all(|r| r.status == "landed"));
        assert!(records[1].verifications[0].reused);
        assert!(records[1].verifications[0].speculative);
        assert_eq!(records[1].landed_sha.as_deref(), Some("M2"));
    }

    #[test]
    fn fail_pass_reverifies_b2_onto_main_then_lands() {
        let mut m = Machine::new("main0", 2);
        let d = m.dispatch(2, &BTreeMap::new());
        let mut busy = busy_of(&d);
        m.prepared(0, "M1".into());
        busy.extend(busy_of(&m.dispatch(2, &busy)));
        // b1 fails; b2's speculative run passes on dead base M1.
        finish(&mut m, 0, 1, "main0", Some("M1"), Verdict::Failed);
        busy.remove(&0);
        assert_eq!(m.next_serial(), Decision::Wait, "b2 still in flight");
        finish(&mut m, 1, 2, "M1", Some("M2dead"), Verdict::Passed);
        busy.clear();
        assert_eq!(m.next_serial(), Decision::Wait, "b2 needs a fresh base");
        // b1 is settled failed; b2's passing attempt is on a dead base, so the
        // dispatcher must re-issue b2 onto main0.
        let d = m.dispatch(2, &busy);
        assert_eq!(
            d,
            vec![Decision::Verify {
                idx: 1,
                lane: 1,
                base: "main0".into(),
                speculative: false,
                attempt: 1,
            }]
        );
        busy.extend(busy_of(&d));
        assert_eq!(m.next_serial(), Decision::Wait);
        finish(&mut m, 1, 1, "main0", Some("M2"), Verdict::Passed);
        busy.clear();
        assert_eq!(
            m.next_serial(),
            Decision::Land {
                idx: 1,
                base: "main0".into(),
                merge: "M2".into()
            }
        );
        m.landed(1, "M2".into());
        let records = m.records(&["wt/a".into(), "wt/b".into()]);
        assert_eq!(records[0].status, "gate_failed");
        assert_eq!(records[1].status, "landed");
        assert_eq!(records[1].verifications.len(), 2);
        assert!(!records[1].verifications[0].reused);
        assert!(records[1].verifications[1].reused);
        assert_eq!(m.next_serial(), Decision::Done);
    }

    #[test]
    fn pass_fail_lands_b1_and_reports_b2_failure() {
        let mut m = Machine::new("main0", 2);
        let mut busy = busy_of(&m.dispatch(2, &BTreeMap::new()));
        m.prepared(0, "M1".into());
        busy.extend(busy_of(&m.dispatch(2, &busy)));
        finish(&mut m, 0, 1, "main0", Some("M1"), Verdict::Passed);
        busy.remove(&0);
        m.landed(0, "M1".into());
        finish(&mut m, 1, 2, "M1", Some("M2"), Verdict::Failed);
        busy.clear();
        assert_eq!(m.next_serial(), Decision::Done);
        let records = m.records(&["wt/a".into(), "wt/b".into()]);
        assert_eq!(records[0].status, "landed");
        assert_eq!(records[1].status, "gate_failed");
        assert_eq!(records[0].landed_sha.as_deref(), Some("M1"));
        assert_eq!(records[1].landed_sha, None);
    }

    #[test]
    fn third_party_base_move_between_verify_and_land_requests_reverify() {
        let mut m = Machine::new("main0", 1);
        let _ = m.dispatch(1, &BTreeMap::new());
        finish(&mut m, 0, 1, "main0", Some("M1"), Verdict::Passed);
        // Someone else advances main before --land runs.
        m.external_move(0, "other1".into());
        assert_eq!(
            m.next_serial(),
            Decision::Wait,
            "stale passing attempt must not land"
        );
        assert_eq!(
            m.dispatch(1, &BTreeMap::new()),
            vec![Decision::Verify {
                idx: 0,
                lane: 1,
                base: "other1".into(),
                speculative: false,
                attempt: 1,
            }]
        );
        // Each further move consumes one re-verification until the budget is
        // exhausted; then the machine reports BaseMoved and the new main.
        finish(&mut m, 0, 1, "other1", Some("M2"), Verdict::Passed);
        m.external_move(0, "other2".into());
        assert_eq!(m.next_serial(), Decision::Wait);
        assert_eq!(
            m.dispatch(1, &BTreeMap::new()),
            vec![Decision::Verify {
                idx: 0,
                lane: 1,
                base: "other2".into(),
                speculative: false,
                attempt: 2,
            }]
        );
        finish(&mut m, 0, 1, "other2", Some("M3"), Verdict::Passed);
        m.external_move(0, "other3".into());
        assert_eq!(
            m.next_serial(),
            Decision::StopBaseMoved {
                idx: 0,
                current_main: "other3".into(),
            }
        );
    }

    #[test]
    fn merge_conflict_failure_has_no_merge_commit() {
        let mut m = Machine::new("main0", 1);
        let _busy = busy_of(&m.dispatch(2, &BTreeMap::new()));
        finish(&mut m, 0, 1, "main0", None, Verdict::Failed);
        assert_eq!(m.next_serial(), Decision::Done);
        let records = m.records(&["wt/a".into()]);
        assert_eq!(records[0].status, "gate_failed");
        assert_eq!(records[0].verifications[0].merge, None);
    }
}
