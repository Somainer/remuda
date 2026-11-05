//! `wake.check` (cb-wake-check, main-agent.md §11.2): run one wake-up check.
//!
//! A wake-up may carry a shell command that decides whether its run happens.
//! The Hub admitted it (on the OA1 axis) and names this Node; the Node:
//!
//! 1. gives the command to `sh -n` on stdin first (dash runs a `-c` string
//!    even under `-n`). A syntax error is reported as `syntax` and nothing
//!    executes;
//! 2. runs it under an outer `sh` ([`OUTER`]) in the lineage's workspace
//!    (and cwd) on this host, under the mandatory timeout, and when it ends
//!    (its exit or the timeout) kills what it started before the report.
//!    Containment is cleanup, not a security boundary: a check runs with the
//!    authority of whoever was admitted to run it (main-agent.md §11.2), so
//!    a process that outlives it gains nothing that check did not have. What
//!    applies, recorded on the run as its `containment` label (see
//!    [`contain`]):
//!    * `cgroup`: the outer shell was verified to be in the leaf of a cgroup
//!      v2 domain made for this check alone under the Node's delegated cgroup.
//!      `cgroup.kill` on the leaf and the domain ends every process of the
//!      check still in either, whatever `setsid`, a double fork or a cleared
//!      environment did. **Residual:** a process that writes its own pid into
//!      the `cgroup.procs` of a writable ancestor (the Node's delegated cgroup)
//!      leaves both, and, if it also clears its marker, detaches and kills its
//!      supervisor, can outlive the report;
//!    * `subreaper` (Linux otherwise): the outer shell is a child subreaper
//!      (`PR_SET_CHILD_SUBREAPER`), so every orphan below it, `setsid`, a
//!      double fork and `nohup … &` included, is reparented to it. Until two
//!      consecutive walks of `/proc` find nothing, the Node SIGKILLs every
//!      live descendant of it and every process of this user carrying the
//!      check's marker (`REMUDA_WAKE_RUN=<runId>.<nonce>` in its
//!      environment); then the recorded process group, then, once the outer
//!      shell is reaped, the group and the marked processes again.
//!      **Residual limit:** a check that deliberately kills its supervisor
//!      (the outer shell, `kill -9 $PPID`) AND detaches a process that also
//!      leaves the process group and clears its environment (`env -i setsid
//!      …`) is out of every reach, and can outlive the report;
//!    * `process-group` (other Unix): only the outer shell's process group
//!      is killed; a check that leaves its group can outlive the report.
//! 3. keeps at most [`WAKE_CHECK_STDOUT_MAX_BYTES`] of stdout (the rest is
//!    read and dropped, so the check never blocks on a full pipe) and never
//!    reads stderr: the report carries only an exit summary.
//!
//! **Environment.** Built from scratch ([`check_env`]), never inherited
//! whole: `PATH` as the Node has it (so a check finds the same tools its
//! workers do), `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`,
//! `LC_ALL`, `LC_CTYPE`, `TZ` and `SSH_AUTH_SOCK` when the Node has them,
//! `TERM=dumb`, and the Hub's `REMUDA_WAKE_*` run variables. Nothing else: no
//! Remuda credential (`REMUDA_TOKEN`, `REMUDA_HUB`, the Node's own token), no
//! provider key, no carrier flag. A check is not a session and acts as
//! nobody.
//!
//! Logs carry the outcome and exit code only, never the command or its
//! output.

mod contain;

use crate::{DevNode, NodeError};
use contain::{CgroupFs, Label, Leaf};
use remuda_protocol::{
    InstanceId, METHOD_WAKE_CHECK, WAKE_CHECK_ENV_PREFIX, WAKE_CHECK_STDOUT_MAX_BYTES,
    WAKE_CHECK_TIMEOUT_MAX_SECS, WakeCheckOutcome, WakeCheckParams, WakeCheckResult, WorkspaceId,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How long `sh -n` may take.
const SYNTAX_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the stdout reader may take after the group is gone.
const STDOUT_GRACE: Duration = Duration::from_secs(2);
/// `PATH` when the Node has none.
const BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The Node variables a check inherits, when set.
const PASSTHROUGH: [&str; 10] = [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
];
/// Passed through as well: a check that fetches over ssh needs the agent.
const PASSTHROUGH_SSH: &str = "SSH_AUTH_SOCK";

/// Whether `method` is the wake-check RPC.
#[must_use]
pub(crate) fn is_wake_check_method(method: &str) -> bool {
    method == METHOD_WAKE_CHECK
}

/// The passthrough variables as this Node process has them.
fn node_env() -> BTreeMap<String, String> {
    PASSTHROUGH
        .iter()
        .copied()
        .chain([PASSTHROUGH_SSH])
        .filter_map(|key| {
            std::env::var_os(key)
                .and_then(|value| value.into_string().ok())
                .map(|value| (key.to_owned(), value))
        })
        .collect()
}

/// The environment a check runs with: the passthrough subset of `node`,
/// `TERM=dumb`, a base `PATH` when `node` has none, and the `REMUDA_WAKE_*`
/// entries of `run` (any other name the Hub sent is dropped).
pub(crate) fn check_env(
    node: &BTreeMap<String, String>,
    run: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = node
        .iter()
        .filter(|(key, _)| PASSTHROUGH.contains(&key.as_str()) || key.as_str() == PASSTHROUGH_SSH)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.entry("PATH".to_owned())
        .or_insert_with(|| BASE_PATH.to_owned());
    env.insert("TERM".to_owned(), "dumb".to_owned());
    for (key, value) in run {
        if key.starts_with(WAKE_CHECK_ENV_PREFIX)
            && key.len() > WAKE_CHECK_ENV_PREFIX.len()
            && key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && !value.contains('\0')
        {
            env.insert(key.clone(), value.clone());
        }
    }
    env
}

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckRun {
    pub(crate) outcome: WakeCheckOutcome,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stdout_truncated: bool,
    pub(crate) summary: String,
    /// How its processes were contained; `None` when nothing ran.
    pub(crate) containment: Option<Label>,
    /// The sweep stopped at its hard limit with processes still live.
    pub(crate) containment_incomplete: bool,
}

impl CheckRun {
    fn without_output(outcome: WakeCheckOutcome, summary: impl Into<String>) -> Self {
        Self {
            outcome,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_truncated: false,
            summary: summary.into(),
            containment: None,
            containment_incomplete: false,
        }
    }
}

/// `sh` with the check's environment, in its own process group, reading
/// nothing and writing stderr nowhere.
fn shell(cwd: &Path, env: &BTreeMap<String, String>) -> Command {
    let mut command = Command::new("sh");
    command
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Kill the process group led by `pgid`, if any of it is left.
fn kill_group(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.and_then(|pid| i32::try_from(pid).ok())
        && pgid > 0
    {
        // ESRCH (nothing left) is the usual answer after a clean exit. The
        // pgid cannot have been reused while any member of the group is
        // alive, and when none is, there is nothing to kill.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Read `pipe` to its end, keeping at most `cap` bytes.
async fn read_bounded(mut pipe: impl tokio::io::AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut total = 0_usize;
    let mut buf = [0_u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                total = total.saturating_add(read);
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..read.min(room)]);
            }
        }
    }
    (kept, total > cap)
}

/// `bytes` as text of at most `cap` bytes, cut at a character boundary.
fn bounded_text(bytes: &[u8], cap: usize) -> (String, bool) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= cap {
        return (text, false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// `sh -n` over `command`: `Ok(true)` when it parses. Nothing executes.
///
/// The script goes in on stdin, never as `-c`: dash ignores `-n` for a
/// `-c` string and runs it, so `sh -n -c <command>` would run the check
/// twice. Whatever happens, the group is killed before this returns.
async fn parses(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<bool> {
    use tokio::io::AsyncWriteExt;
    let mut sh = shell(cwd, env);
    sh.arg("-n").stdin(Stdio::piped()).stdout(Stdio::null());
    let mut child = sh.spawn()?;
    let pgid = child.id();
    let script = command.as_bytes().to_vec();
    let checked = tokio::time::timeout(SYNTAX_TIMEOUT, async {
        if let Some(mut stdin) = child.stdin.take() {
            // A shell that stops reading early (a syntax error) closes the
            // pipe: its exit status is the answer, not the write error.
            let _ = stdin.write_all(&script).await;
            let _ = stdin.write_all(b"\n").await;
            drop(stdin);
        }
        child.wait().await
    })
    .await;
    kill_group(pgid);
    match checked {
        Ok(status) => Ok(status?.success()),
        Err(_) => {
            let _ = child.wait().await;
            Ok(false)
        }
    }
}

/// The outer shell a check runs under. It runs the check as its own child
/// (`sh -c "$1"`, stdin and stderr `/dev/null`, stdout the Node's pipe),
/// reports the exit status on its own stderr (the Node's control pipe), then
/// waits on its stdin until the Node has killed every descendant and closes
/// it. Being the subreaper, it adopts every orphan of the check meanwhile.
const OUTER: &str =
    "sh -c \"$1\" </dev/null 2>/dev/null; echo \"$?\" >&2; read _ 2>/dev/null; exit 0";
/// While the sweep still finds processes, it pauses between kill passes until
/// this long after it started; past it, it keeps killing without pausing.
const SWEEP_SOFT: Duration = Duration::from_secs(5);
/// The sweep stops with processes still live only this long after it started:
/// a process the kernel cannot kill (uninterruptible sleep) must not hold the
/// report forever. Such a sweep is logged as incomplete.
const SWEEP_HARD: Duration = Duration::from_secs(15);
/// The pause between kill passes before [`SWEEP_SOFT`].
const KILL_GAP: Duration = Duration::from_millis(10);
/// The delay between the two consecutive empty walks that end a sweep: a
/// fork in flight shows up in the second.
const EMPTY_GAP: Duration = Duration::from_millis(25);
/// How long the outer shell may take to exit once released.
const OUTER_EXIT_GRACE: Duration = Duration::from_secs(2);
/// The longest a `wake.check` call can take beyond the check's own timeout:
/// the `sh -n` precheck, the descendant sweep, the outer shell's exit and the
/// stdout drain. The Hub waits longer than the timeout plus this.
pub(crate) const MAX_OVERHEAD: Duration = Duration::from_secs(
    SYNTAX_TIMEOUT.as_secs()
        + SWEEP_HARD.as_secs()
        + OUTER_EXIT_GRACE.as_secs()
        + STDOUT_GRACE.as_secs()
        + 1,
);

// The Hub sizes its wait from the protocol's bound; the Node stays inside it.
const _: () = assert!(MAX_OVERHEAD.as_secs() <= remuda_protocol::WAKE_CHECK_NODE_OVERHEAD_SECS);

/// What contained one check: a verified cgroup leaf, or none.
#[derive(Debug, Default)]
struct Containment {
    /// The cgroup v2 leaf the check's outer shell was verified to have joined.
    leaf: Option<Leaf>,
}

impl Containment {
    fn label(&self) -> Label {
        if self.leaf.is_some() {
            Label::Cgroup
        } else {
            Label::without_leaf()
        }
    }
}

/// A fresh cgroup v2 leaf for one check, when the Node's own cgroup is
/// delegated to it (see [`CgroupFs::leaf_parent`]); not yet joined.
fn cgroup_leaf() -> Option<Leaf> {
    CgroupFs::system()?.create_leaf(&uuid::Uuid::now_v7().simple().to_string())
}

/// The live (not zombie) descendants of `root`, from `/proc/*/stat`.
#[cfg(target_os = "linux")]
fn live_descendants(root: u32) -> Vec<i32> {
    let mut children: BTreeMap<i32, Vec<(i32, bool)>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The fields after the parenthesised command name: state, ppid, …
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next().unwrap_or("");
        let Some(ppid) = fields.next().and_then(|ppid| ppid.parse::<i32>().ok()) else {
            continue;
        };
        children.entry(ppid).or_default().push((pid, state != "Z"));
    }
    let mut found = Vec::new();
    let mut queue = vec![i32::try_from(root).unwrap_or(-1)];
    while let Some(parent) = queue.pop() {
        for (pid, live) in children.get(&parent).into_iter().flatten() {
            if *live {
                found.push(*pid);
            }
            queue.push(*pid);
        }
    }
    found
}

/// The live processes of process group `pgid` owned by this user.
#[cfg(target_os = "linux")]
fn live_group_members(pgid: i32) -> Vec<i32> {
    let uid = nix::unistd::getuid().as_raw();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if !owned_by(&entry.path(), uid) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        // state, ppid, pgrp, …
        let fields: Vec<&str> = rest.split_whitespace().take(3).collect();
        if fields.len() == 3 && fields[0] != "Z" && fields[2].parse::<i32>().ok() == Some(pgid) {
            found.push(pid);
        }
    }
    found
}

#[cfg(target_os = "linux")]
fn owned_by(proc_dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(proc_dir).is_ok_and(|meta| meta.uid() == uid)
}

/// The environment variable that marks every process of one check.
const MARKER_VAR: &str = "REMUDA_WAKE_RUN";

/// This user's live processes (other than the Node) whose environment holds
/// `REMUDA_WAKE_RUN=<marker>`: a check's processes, however they detached,
/// unless they cleared their own environment.
#[cfg(target_os = "linux")]
fn live_marked(marker: &str) -> Vec<i32> {
    let uid = nix::unistd::getuid().as_raw();
    let own = i32::try_from(std::process::id()).unwrap_or(-1);
    let needle = format!("{MARKER_VAR}={marker}").into_bytes();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == own || !owned_by(&entry.path(), uid) {
            continue;
        }
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        if !environ
            .split(|byte| *byte == 0)
            .any(|var| var == needle.as_slice())
        {
            continue;
        }
        let zombie = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.split_whitespace().next() == Some("Z"))
            })
            .unwrap_or(true);
        if !zombie {
            found.push(pid);
        }
    }
    found
}

/// The pids in a cgroup leaf and its per-check domain.
fn leaf_members(containment: &Containment) -> Vec<i32> {
    containment
        .leaf
        .as_ref()
        .map(Leaf::members)
        .unwrap_or_default()
}

/// Kills what a find returns until the find is stably empty.
struct Sweep {
    started: tokio::time::Instant,
    soft: Duration,
    hard: Duration,
    /// A sweep stopped at its hard limit with processes still live.
    incomplete: bool,
}

impl Sweep {
    fn new() -> Self {
        Self::with_limits(SWEEP_SOFT, SWEEP_HARD)
    }

    fn with_limits(soft: Duration, hard: Duration) -> Self {
        Self {
            started: tokio::time::Instant::now(),
            soft,
            hard,
            incomplete: false,
        }
    }

    /// SIGKILL every pid `find` returns, pass after pass, until two
    /// consecutive walks find nothing, [`EMPTY_GAP`] apart. While it still
    /// finds processes it never stops at [`SWEEP_SOFT`] (which only ends the
    /// pauses between kill passes), only at [`SWEEP_HARD`].
    async fn until_stable(&mut self, find: impl Fn() -> Vec<i32>) {
        let mut empty = 0;
        loop {
            let live = find();
            if live.is_empty() {
                empty += 1;
                if empty >= 2 {
                    return;
                }
                tokio::time::sleep(EMPTY_GAP).await;
                continue;
            }
            empty = 0;
            for pid in live {
                #[cfg(unix)]
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
                #[cfg(not(unix))]
                let _ = pid;
            }
            let elapsed = self.started.elapsed();
            if elapsed >= self.hard {
                self.incomplete = true;
                tracing::warn!(
                    category = "wake-check",
                    "processes of a check survived SIGKILL for the sweep's hard limit"
                );
                return;
            }
            if elapsed < self.soft {
                tokio::time::sleep(KILL_GAP).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Kill everything the check started, whatever `setsid`, a double fork or
/// `nohup` did, while the outer shell is still alive: `cgroup.kill` on a
/// verified leaf, then [`Sweep::until_stable`] over the leaf's members and
/// every live descendant of the outer shell (the subreaper adopts each
/// orphan, so none is out of its reach). The outer shell itself is left alive
/// for its release: were it killed first, its orphans would go to init, out
/// of the walk's reach. Elsewhere: the process group.
async fn sweep(sweep: &mut Sweep, containment: &Containment, outer: Option<u32>, marker: &str) {
    if let Some(leaf) = &containment.leaf {
        leaf.kill();
    }
    #[cfg(not(target_os = "linux"))]
    kill_group(outer);
    #[cfg(target_os = "linux")]
    {
        let outer = outer.and_then(|pid| i32::try_from(pid).ok());
        sweep
            .until_stable(|| {
                let mut live = leaf_members(containment);
                live.extend(live_marked(marker));
                if let Some(outer) = outer {
                    live.retain(|pid| *pid != outer);
                    live.extend(live_descendants(outer.unsigned_abs()));
                }
                live
            })
            .await;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sweep, containment, marker);
}

/// After the outer shell is reaped: one more sweep, so anything reparented
/// in its exit window is killed before the report. The outer shell's process
/// group (its pid) is walked: a group's id stays reserved while any member
/// lives, and only this user's processes are taken.
async fn sweep_after_reap(
    sweep: &mut Sweep,
    containment: &Containment,
    outer: Option<u32>,
    marker: &str,
) {
    #[cfg(target_os = "linux")]
    {
        let group = outer.and_then(|pid| i32::try_from(pid).ok());
        sweep
            .until_stable(|| {
                let mut live = leaf_members(containment);
                live.extend(live_marked(marker));
                if let Some(group) = group {
                    live.extend(live_group_members(group));
                }
                live
            })
            .await;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (sweep, containment, outer, marker);
}

/// Remove a cgroup leaf once nothing is left in it.
fn remove_leaf(containment: &Containment) {
    if let Some(leaf) = &containment.leaf {
        for _ in 0..100 {
            if leaf.remove() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        tracing::warn!(
            category = "wake-check",
            "a check's cgroup leaf could not be removed"
        );
    }
}

/// What the sweep established, in words that claim no more than it did: an
/// incomplete sweep left processes live, and each containment has its own
/// residual (see [`contain`]).
fn swept_summary(label: Label, incomplete: bool) -> String {
    if incomplete {
        return format!(
            "its processes were swept ({}), but some were still live when the sweep stopped",
            label.as_str()
        );
    }
    let residual = match label {
        Label::Cgroup => "a process that moved itself out of its cgroup could have escaped",
        Label::Subreaper => {
            "a process that killed its supervisor and cleared its environment could have escaped"
        }
        Label::ProcessGroup => "a process that left its process group could have escaped",
    };
    format!("its processes were swept ({}); {residual}", label.as_str())
}

/// Run one check: `sh -n`, then the command under `timeout`, contained.
pub(crate) async fn run_check(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    marker: &str,
) -> CheckRun {
    use tokio::io::AsyncBufReadExt;
    // r3: every process of the check inherits the marker unless it clears its
    // own environment; the sweeps find detached ones by it.
    let mut env = env.clone();
    env.insert(MARKER_VAR.to_owned(), marker.to_owned());
    let env = &env;
    match parses(command, cwd, env).await {
        Ok(true) => {}
        Ok(false) => {
            return CheckRun::without_output(
                WakeCheckOutcome::Syntax,
                "syntax error (sh -n); nothing ran",
            );
        }
        Err(_) => {
            return CheckRun::without_output(WakeCheckOutcome::Unavailable, "could not start sh");
        }
    }
    let leaf = cgroup_leaf();
    let mut sh = shell(cwd, env);
    sh.args(["-c", OUTER, "sh", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    remuda_unix::ContainOnExec {
        cgroup: leaf.as_ref().map(|leaf| leaf.dir.join("cgroup.procs")),
        subreaper: true,
    }
    .install(sh.as_std_mut());
    let mut child = match sh.spawn() {
        Ok(child) => child,
        Err(_) => {
            remove_leaf(&Containment { leaf });
            return CheckRun::without_output(WakeCheckOutcome::Unavailable, "could not start sh");
        }
    };
    let outer = child.id();
    // r3: the label is what happened, not what was tried. The pre-exec join
    // can fail (EBUSY, EACCES, a short write) without failing the spawn, so
    // the leaf is kept only when the outer shell is verified to be in it.
    let containment = Containment {
        leaf: leaf.and_then(|leaf| {
            let member =
                outer.and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok());
            contain::confirm(leaf, member.as_deref())
        }),
    };
    let release = child.stdin.take();
    let reader = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(read_bounded(pipe, WAKE_CHECK_STDOUT_MAX_BYTES)));
    // The outer shell's one line: the check's exit status.
    let reported = async {
        let mut line = String::new();
        let control = child.stderr.take()?;
        tokio::io::BufReader::new(control)
            .read_line(&mut line)
            .await
            .ok()?;
        line.trim().parse::<i32>().ok()
    };
    let waited = tokio::time::timeout(timeout, reported).await;
    let mut swept = Sweep::new();
    sweep(&mut swept, &containment, outer, marker).await;
    // Released, the outer shell exits, and its whole recorded process group
    // goes with it (r3: on Linux too); the shell is killed if it lingers.
    drop(release);
    kill_group(outer);
    if tokio::time::timeout(OUTER_EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    sweep_after_reap(&mut swept, &containment, outer, marker).await;
    remove_leaf(&containment);
    let (stdout, cut) = match reader {
        Some(reader) => match tokio::time::timeout(STDOUT_GRACE, reader).await {
            Ok(Ok(read)) => read,
            _ => (Vec::new(), false),
        },
        None => (Vec::new(), false),
    };
    let (stdout, truncated) = bounded_text(&stdout, WAKE_CHECK_STDOUT_MAX_BYTES);
    let stdout_truncated = cut || truncated;
    tracing::debug!(
        category = "wake-check",
        containment = containment.label().as_str(),
        "check contained"
    );
    let label = containment.label();
    let incomplete = swept.incomplete;
    let exit_code = match waited {
        Err(_) => {
            return CheckRun {
                outcome: WakeCheckOutcome::TimedOut,
                exit_code: None,
                signal: None,
                stdout,
                stdout_truncated,
                summary: format!(
                    "timed out after {} s; {}",
                    timeout.as_secs(),
                    swept_summary(label, incomplete)
                ),
                containment: Some(label),
                containment_incomplete: incomplete,
            };
        }
        Ok(None) => {
            return CheckRun {
                containment: Some(label),
                containment_incomplete: incomplete,
                ..CheckRun::without_output(
                    WakeCheckOutcome::Unavailable,
                    format!(
                        "its exit was not seen (its supervisor shell ended first); {}",
                        swept_summary(label, incomplete)
                    ),
                )
            };
        }
        Ok(Some(code)) => code,
    };
    // A shell reports a command a signal ended as 128 + the signal.
    let summary = if exit_code > 128 {
        format!("exit {exit_code} (a signal ends a command with 128 + its number)")
    } else {
        format!("exit {exit_code}")
    };
    CheckRun {
        outcome: WakeCheckOutcome::Exited,
        exit_code: Some(exit_code),
        signal: None,
        stdout,
        stdout_truncated,
        summary,
        containment: Some(label),
        containment_incomplete: incomplete,
    }
}

impl DevNode {
    /// Handle `wake.check`.
    pub(crate) async fn wake_check_rpc(&self, params: &Value) -> Result<Value, NodeError> {
        let request: WakeCheckParams = serde_json::from_value(params.clone()).map_err(|error| {
            NodeError::InvalidRequest(format!("invalid wake.check params: {error}"))
        })?;
        let max = u64::try_from(WAKE_CHECK_TIMEOUT_MAX_SECS).unwrap_or(3600);
        if request.timeout_secs == 0 || request.timeout_secs > max {
            return Err(NodeError::InvalidRequest(format!(
                "wake.check timeoutSecs must be between 1 and {max}"
            )));
        }
        // The lineage's workspace on this host: the one the Hub names, else
        // that of the lineage's chapter here. Never one the Node picks: with
        // neither, the check is refused and nothing runs.
        let workspace = match (&request.workspace_id, &request.instance_id) {
            (Some(workspace), _) => WorkspaceId::from_str(workspace).ok(),
            (None, Some(instance)) => InstanceId::from_str(instance)
                .ok()
                .and_then(|instance| self.get_instance(&instance).ok())
                .map(|instance| instance.workspace_id),
            (None, None) => None,
        };
        // The workspace first (it must be registered here), then the
        // directory in it: an unusable cwd is its own failure (r3).
        let place = match workspace
            .filter(|workspace| self.resolve_workspace_cwd(Some(workspace), None).is_ok())
        {
            None => Err(WakeCheckOutcome::NoWorkspace),
            Some(workspace) => self
                .resolve_workspace_cwd(Some(&workspace), request.cwd.as_deref())
                .map_err(|_| WakeCheckOutcome::CwdUnusable),
        };
        let run = match place {
            Ok((_, cwd)) => {
                let env = check_env(&node_env(), &request.env);
                // Unique per call, even were the Hub to ask twice for a run.
                let marker = format!("{}.{}", request.run_id, uuid::Uuid::now_v7().simple());
                run_check(
                    &request.command,
                    &cwd,
                    &env,
                    Duration::from_secs(request.timeout_secs),
                    &marker,
                )
                .await
            }
            Err(WakeCheckOutcome::CwdUnusable) => CheckRun::without_output(
                WakeCheckOutcome::CwdUnusable,
                "its directory in the lineage's workspace is not usable; nothing ran",
            ),
            Err(_) => CheckRun::without_output(
                WakeCheckOutcome::NoWorkspace,
                "no workspace of the lineage on this Node; nothing ran",
            ),
        };
        tracing::info!(
            category = "wake-check",
            outcome = ?run.outcome,
            exit_code = run.exit_code,
            signal = run.signal,
            "wake check finished"
        );
        serde_json::to_value(WakeCheckResult {
            run_id: request.run_id,
            outcome: run.outcome,
            exit_code: run.exit_code,
            signal: run.signal,
            stdout: run.stdout,
            stdout_truncated: run.stdout_truncated,
            summary: run.summary,
            containment: run.containment.map(Label::wire),
            containment_incomplete: run.containment_incomplete,
        })
        .map_err(NodeError::from)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        check_env(&node_env(), &BTreeMap::new())
    }

    async fn run(dir: &Path, command: &str, secs: u64) -> CheckRun {
        let marker = format!("test.{}", uuid::Uuid::now_v7().simple());
        run_check(command, dir, &env(), Duration::from_secs(secs), &marker).await
    }

    /// Whether `pid` is still a live (not zombie) process.
    fn alive(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // The state follows the parenthesised command name.
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z"),
            Err(_) => false,
        }
    }

    /// Bounded wait (in polls, not a timing assertion) for `pid` to die:
    /// a killed orphan is reaped by init on its own schedule.
    async fn gone(pid: i32) -> bool {
        for _ in 0..500 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn pid_in(path: &Path) -> i32 {
        std::fs::read_to_string(path)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    #[tokio::test]
    async fn exit_codes_and_stdout_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let passed = run(dir.path(), "printf 'queue: 3 jobs\\n'; exit 0", 30).await;
        // r4: the label is checked against evidence the containment code does
        // not produce: the cgroup the kernel says the check's own shell was
        // in, and the filesystem mounted at /sys/fs/cgroup.
        let observed = run(dir.path(), "cat /proc/self/cgroup", 30).await;
        let in_check_leaf = observed.stdout.lines().any(|line| {
            line.strip_prefix("0::")
                .is_some_and(|path| path.contains("/remuda-wake-") && path.ends_with("/check"))
        });
        let expected = if in_check_leaf {
            Label::Cgroup
        } else if cfg!(target_os = "linux") {
            Label::Subreaper
        } else {
            Label::ProcessGroup
        };
        assert_eq!(observed.containment, Some(expected), "{}", observed.stdout);
        assert_eq!(passed.containment, Some(expected));
        let cgroup2 = std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap_or_default()
            .lines()
            .any(|line| {
                line.split(' ').nth(4) == Some("/sys/fs/cgroup")
                    && line
                        .split(" - ")
                        .nth(1)
                        .is_some_and(|rest| rest.starts_with("cgroup2 "))
            });
        if !cgroup2 {
            assert!(!in_check_leaf, "no cgroup2 mount, no leaf");
            assert_eq!(passed.containment, Some(Label::Subreaper));
        }
        assert_eq!(passed.outcome, WakeCheckOutcome::Exited);
        assert_eq!(passed.exit_code, Some(0));
        assert_eq!(passed.stdout, "queue: 3 jobs\n");
        assert_eq!(passed.summary, "exit 0");
        let skipped = run(dir.path(), "echo nothing to do; exit 2", 30).await;
        assert_eq!(skipped.exit_code, Some(2));
        let failed = run(dir.path(), "echo oops >&2; exit 1", 30).await;
        assert_eq!(failed.exit_code, Some(1));
        assert_eq!(failed.stdout, "", "stderr is never kept");
        // It runs in the directory it was given.
        let here = run(dir.path(), "pwd", 30).await;
        assert_eq!(
            Path::new(here.stdout.trim()).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn a_syntax_error_fails_and_nothing_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let checked = run(
            dir.path(),
            &format!("touch {}; if then", marker.display()),
            30,
        )
        .await;
        assert_eq!(checked.outcome, WakeCheckOutcome::Syntax);
        assert_eq!(checked.exit_code, None);
        assert_eq!(checked.containment, None, "nothing ran");
        assert!(!marker.exists(), "sh -n executes nothing");
    }

    /// The precheck never runs the command: a check that parses runs once
    /// (dash runs a `-c` string even under `-n`).
    #[tokio::test]
    async fn a_check_that_parses_runs_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("runs");
        let checked = run(
            dir.path(),
            &format!("echo run >> {}; exit 2", counter.display()),
            30,
        )
        .await;
        assert_eq!(checked.exit_code, Some(2));
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "run\n");
    }

    #[tokio::test]
    async fn the_timeout_kills_the_whole_group_and_leaves_no_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let checked = run(
            dir.path(),
            &format!(
                "sleep 600 & echo $! > {}; echo started; sleep 600",
                pidfile.display()
            ),
            1,
        )
        .await;
        assert_eq!(checked.outcome, WakeCheckOutcome::TimedOut);
        assert_eq!(checked.exit_code, None);
        assert_eq!(checked.stdout, "started\n");
        assert!(
            gone(pid_in(&pidfile)).await,
            "the background sleep survived"
        );
    }

    /// cb-wake-check r2: `setsid`, a double fork and `nohup … &` all leave
    /// the shell's process group; none of them outlives the report, on the
    /// timeout or after a clean exit.
    #[tokio::test]
    async fn no_escape_by_setsid_double_fork_or_nohup_outlives_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = |name: &str| dir.path().join(name);
        let escapes = format!(
            "setsid sleep 600 & echo $! > {a}; \
             (setsid sh -c 'sleep 600 & echo $! > {b}') & \
             nohup sleep 600 >/dev/null 2>&1 & echo $! > {c}; \
             (sh -c 'setsid sh -c \"sleep 600 & echo \\$! > {d}\"' &) ; \
             i=0; while [ ! -s {b} ] || [ ! -s {d} ]; do i=$((i+1)); [ $i -gt 500 ] && break; sleep 0.01; done",
            a = pid_file("a").display(),
            b = pid_file("b").display(),
            c = pid_file("c").display(),
            d = pid_file("d").display(),
        );
        for (tail, secs, outcome) in [
            ("; exit 0", 30, WakeCheckOutcome::Exited),
            ("; sleep 600", 2, WakeCheckOutcome::TimedOut),
        ] {
            for name in ["a", "b", "c", "d"] {
                let _ = std::fs::remove_file(pid_file(name));
            }
            let checked = run(dir.path(), &format!("{escapes}{tail}"), secs).await;
            assert_eq!(checked.outcome, outcome, "{tail}: {checked:?}");
            for name in ["a", "b", "c", "d"] {
                let pid = pid_in(&pid_file(name));
                // Right after the report: no bounded wait here.
                assert!(
                    !alive(pid),
                    "{tail}: the {name} escapee {pid} survived the report"
                );
            }
        }
    }

    /// Live (not zombie) processes whose cwd is `dir`: whatever a check run
    /// there left behind, however it detached.
    fn survivors(dir: &Path) -> Vec<String> {
        let dir = dir.canonicalize().unwrap();
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if std::fs::read_link(entry.path().join("cwd")).ok().as_deref() == Some(dir.as_path())
                && alive(pid)
            {
                let cmdline = std::fs::read_to_string(entry.path().join("cmdline"))
                    .unwrap_or_default()
                    .replace('\0', " ");
                found.push(format!("{pid} {cmdline}"));
            }
        }
        found
    }

    /// Kill what a failed assertion would otherwise leave running.
    fn reap_survivors(dir: &Path) {
        for line in survivors(dir) {
            if let Some(pid) = line.split(' ').next().and_then(|pid| pid.parse().ok()) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    /// r3: the sweep's own rules, with a scripted find: an empty walk does
    /// not end it (a second one must follow), and past the soft limit it
    /// keeps killing what it still finds.
    #[tokio::test]
    async fn the_sweep_needs_two_empty_walks_and_never_stops_on_a_live_set() {
        let spawn = || {
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        let mut third = spawn();
        let pids: Vec<i32> = [&first, &second, &third]
            .iter()
            .map(|child| i32::try_from(child.id()).unwrap())
            .collect();
        // Walks: nothing; the first process; nothing; the second (after the
        // soft limit of 0); the third; then nothing, twice.
        let script = std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            vec![],
            vec![pids[0]],
            vec![],
            vec![pids[1]],
            vec![pids[2]],
        ]));
        let mut sweep = Sweep::with_limits(Duration::ZERO, Duration::from_secs(30));
        sweep
            .until_stable(|| script.lock().unwrap().pop_front().unwrap_or_default())
            .await;
        let killed: Vec<bool> = [&mut first, &mut second, &mut third]
            .into_iter()
            .map(|child| {
                child.try_wait().unwrap().is_some() || {
                    std::thread::sleep(Duration::from_millis(200));
                    child.try_wait().unwrap().is_some()
                }
            })
            .collect();
        for child in [&mut first, &mut second, &mut third] {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(killed, [true, true, true]);
        assert!(!sweep.incomplete);
        assert!(script.lock().unwrap().is_empty(), "every walk was made");
    }

    /// r3: a check that kills its supervisor leaves its plain children to
    /// init; they are still in the outer shell's process group, which the
    /// sweep walks once more after the outer shell is reaped.
    #[tokio::test]
    async fn children_orphaned_by_a_killed_supervisor_are_swept_by_group() {
        let dir = tempfile::tempdir().unwrap();
        let checked = run(dir.path(), "sleep 600 & kill -9 $PPID; sleep 600", 30).await;
        let left = survivors(dir.path());
        reap_survivors(dir.path());
        assert!(left.is_empty(), "{checked:?} left {left:?}");
    }

    /// r3: a detached process orphaned by a check that killed its
    /// supervisor is in no tree and no group the Node knows; it carries the
    /// check's marker, and the marker sweep kills it.
    #[tokio::test]
    async fn the_marker_sweep_kills_what_a_supervisor_killer_detached() {
        let dir = tempfile::tempdir().unwrap();
        let checked = run(
            dir.path(),
            "setsid sleep 600 & echo $! > f; (setsid sh -c 'sleep 600 &') ; kill -9 $PPID; sleep 600",
            30,
        )
        .await;
        let left = survivors(dir.path());
        reap_survivors(dir.path());
        assert!(left.is_empty(), "{checked:?} left {left:?}");
        // The marker only matches its own check's processes.
        assert!(live_marked("never-issued").is_empty());
    }

    /// r3: a fork in flight when the check exits (the setsid child has not
    /// forked its own child yet), and a short fork storm, leave nothing
    /// behind: the sweep ends only after two consecutive empty walks.
    #[tokio::test]
    async fn a_fork_in_flight_or_a_fork_storm_leaves_nothing_behind() {
        for (name, command) in [
            (
                "in-flight",
                "setsid sh -c 'sleep 600 & echo $! > f'; exit 0",
            ),
            (
                "in-flight-timeout",
                "setsid sh -c 'sleep 0.3; sleep 600 & echo $! > f' & sleep 600",
            ),
            (
                "storm",
                "(while :; do setsid sleep 600 & done) & sleep 0.3; exit 0",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let secs = if name.ends_with("timeout") { 1 } else { 30 };
            let checked = run(dir.path(), command, secs).await;
            let left = survivors(dir.path());
            reap_survivors(dir.path());
            assert!(left.is_empty(), "{name}: {checked:?} left {left:?}");
        }
    }

    #[tokio::test]
    async fn a_process_left_behind_after_the_exit_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let checked = run(
            dir.path(),
            &format!("sleep 600 & echo $! > {}; exit 0", pidfile.display()),
            30,
        )
        .await;
        assert_eq!(checked.exit_code, Some(0));
        assert!(gone(pid_in(&pidfile)).await, "the straggler survived");
    }

    #[tokio::test]
    async fn stdout_is_bounded_and_drained() {
        let dir = tempfile::tempdir().unwrap();
        let checked = run(dir.path(), "head -c 200000 /dev/zero | tr '\\000' a", 30).await;
        assert_eq!(checked.exit_code, Some(0), "a full pipe never blocks it");
        assert_eq!(checked.stdout.len(), WAKE_CHECK_STDOUT_MAX_BYTES);
        assert!(checked.stdout_truncated);
        // A multi-byte character across the cut is dropped whole.
        let wide = run(
            dir.path(),
            "printf x; i=0; while [ $i -lt 3000 ]; do printf 'é'; i=$((i+1)); done",
            30,
        )
        .await;
        assert!(wide.stdout.len() <= WAKE_CHECK_STDOUT_MAX_BYTES);
        assert!(wide.stdout.starts_with('x'));
        assert!(
            wide.stdout.chars().skip(1).all(|ch| ch == 'é'),
            "{:?}",
            wide.stdout
        );
        assert!(wide.stdout_truncated);
    }

    /// Every carrier routes `wake.check` through the shared dispatch table;
    /// it runs in the named workspace and cwd, with the run's variables.
    #[tokio::test]
    async fn the_dispatch_table_runs_a_check_in_the_lineage_workspace() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(project.clone())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf());
        let node = DevNode::new(&config).unwrap();
        let workspace = node.workspaces().unwrap()[0].meta.id.clone();
        let result = crate::transport::hubnode::dispatch_method(
            &node,
            METHOD_WAKE_CHECK,
            serde_json::json!({
                "runId": "wrn_1", "command": "pwd; echo \"$REMUDA_WAKE_RUN_ID\"; exit 2",
                "timeoutSecs": 30, "workspaceId": workspace, "cwd": "sub",
                "env": {"REMUDA_WAKE_RUN_ID": "wrn_1"},
            }),
        )
        .await
        .unwrap();
        let result: WakeCheckResult = serde_json::from_value(result).unwrap();
        assert_eq!(result.run_id, "wrn_1");
        assert_eq!(result.outcome, WakeCheckOutcome::Exited);
        assert!(
            result.containment.is_some(),
            "r3: the wire result names its containment"
        );
        assert_eq!(result.exit_code, Some(2));
        let mut lines = result.stdout.lines();
        assert_eq!(
            Path::new(lines.next().unwrap()).canonicalize().unwrap(),
            project.join("sub").canonicalize().unwrap()
        );
        assert_eq!(lines.next(), Some("wrn_1"));
        // A workspace this Node does not have: the check could not start.
        let missing = crate::transport::hubnode::dispatch_method(
            &node,
            METHOD_WAKE_CHECK,
            serde_json::json!({
                "runId": "wrn_2", "command": "true", "timeoutSecs": 30,
                "workspaceId": WorkspaceId::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(missing["outcome"], "no-workspace");
        // r2: neither a workspace nor a chapter here: refused, and nothing
        // runs in the Node's first workspace.
        let marker = project.join("ran");
        for params in [
            serde_json::json!({"runId": "wrn_4", "command": format!("touch {}", marker.display()),
                               "timeoutSecs": 30}),
            serde_json::json!({"runId": "wrn_5", "command": format!("touch {}", marker.display()),
                               "timeoutSecs": 30, "instanceId": remuda_protocol::InstanceId::new()}),
        ] {
            let refused =
                crate::transport::hubnode::dispatch_method(&node, METHOD_WAKE_CHECK, params)
                    .await
                    .unwrap();
            assert_eq!(refused["outcome"], "no-workspace", "{refused}");
        }
        assert!(!marker.exists(), "nothing ran in the first workspace");
        // r3: the workspace is there, its directory is not: its own outcome.
        for cwd in ["missing", "../../outside"] {
            let refused = crate::transport::hubnode::dispatch_method(
                &node,
                METHOD_WAKE_CHECK,
                serde_json::json!({"runId": "wrn_6", "command": format!("touch {}", marker.display()),
                                   "timeoutSecs": 30, "workspaceId": workspace, "cwd": cwd}),
            )
            .await
            .unwrap();
            assert_eq!(refused["outcome"], "cwd-unusable", "{cwd}: {refused}");
        }
        assert!(!marker.exists());
        for timeout in [0, 3601] {
            assert!(
                crate::transport::hubnode::dispatch_method(
                    &node,
                    METHOD_WAKE_CHECK,
                    serde_json::json!({"runId": "wrn_3", "command": "true",
                                       "timeoutSecs": timeout}),
                )
                .await
                .is_err(),
                "{timeout}"
            );
        }
        node.shutdown().await.unwrap();
    }

    /// r3: with no workspace id, a check that names the lineage's chapter on
    /// this Node runs in the workspace that instance runs in (here the second
    /// one, so a first-workspace fallback would show), in the given cwd.
    #[tokio::test]
    async fn a_chapter_instance_id_resolves_its_own_workspace() {
        let root = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let extra = root.path().join("extra");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(extra.join("src")).unwrap();
        let config = crate::DevServerConfig::loopback(0)
            .with_workspace_root(project.clone())
            .with_workspace_roots(vec![root.path().to_path_buf()])
            .with_workspace_registry(data.path().to_path_buf())
            .with_workspaces(vec![extra.clone()]);
        let node = DevNode::new(&config).unwrap();
        let workspaces = node.workspaces().unwrap();
        let second = workspaces
            .iter()
            .find(|workspace| Path::new(&workspace.root_path) == extra.canonicalize().unwrap())
            .or_else(|| workspaces.get(1))
            .unwrap()
            .meta
            .id
            .clone();
        assert_ne!(workspaces[0].meta.id, second, "not the first workspace");
        let created = crate::transport::hubnode::dispatch_method(
            &node,
            "instance.create",
            serde_json::json!({"spec": {"workspaceId": second, "driver": "claude-print"}}),
        )
        .await
        .unwrap();
        let instance = created["instance"]["id"].as_str().unwrap().to_owned();
        assert_eq!(
            created["instance"]["workspaceId"],
            serde_json::json!(second)
        );
        let result = crate::transport::hubnode::dispatch_method(
            &node,
            METHOD_WAKE_CHECK,
            serde_json::json!({"runId": "wrn_7", "command": "pwd; exit 2", "timeoutSecs": 30,
                               "instanceId": instance, "cwd": "src"}),
        )
        .await
        .unwrap();
        let result: WakeCheckResult = serde_json::from_value(result).unwrap();
        assert_eq!(result.exit_code, Some(2), "{result:?}");
        assert_eq!(
            Path::new(result.stdout.trim()).canonicalize().unwrap(),
            extra.join("src").canonicalize().unwrap()
        );
        node.shutdown().await.unwrap();
    }

    /// r4: the summary never claims more than the sweep established.
    #[test]
    fn the_swept_summary_follows_the_sweep() {
        let incomplete = swept_summary(Label::Subreaper, true);
        assert!(incomplete.contains("still live"), "{incomplete}");
        for label in [Label::Cgroup, Label::Subreaper, Label::ProcessGroup] {
            let complete = swept_summary(label, false);
            assert!(complete.contains(label.as_str()), "{complete}");
            assert!(complete.contains("could have escaped"), "{complete}");
            assert!(!complete.contains("every process"), "{complete}");
        }
    }

    #[test]
    fn the_environment_is_the_named_subset_and_the_run_variables() {
        let node: BTreeMap<String, String> = [
            ("PATH", "/node/bin:/usr/bin"),
            ("HOME", "/home/n"),
            ("REMUDA_TOKEN", "token"),
            ("REMUDA_HUB", "http://hub"),
            ("REMUDA_HOST_TOKEN", "enroll"),
            ("ANTHROPIC_API_KEY", "key"),
            ("REMUDA_PTY_EMULATOR", "1"),
            ("SSH_AUTH_SOCK", "/run/agent"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let run: BTreeMap<String, String> = [
            ("REMUDA_WAKE_RUN_ID", "wrn_1"),
            ("REMUDA_WAKE_LAST_RUN_AT", "2026-11-04T10:00:00.000Z"),
            ("REMUDA_TOKEN", "smuggled"),
            ("REMUDA_WAKE_", "empty name"),
            ("REMUDA_WAKE_lower", "odd name"),
            ("PATH", "/evil"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let env = check_env(&node, &run);
        let keys: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "HOME",
                "PATH",
                "REMUDA_WAKE_LAST_RUN_AT",
                "REMUDA_WAKE_RUN_ID",
                "SSH_AUTH_SOCK",
                "TERM"
            ]
        );
        assert_eq!(env["PATH"], "/node/bin:/usr/bin");
        assert_eq!(env["TERM"], "dumb");
        assert_eq!(
            check_env(&BTreeMap::new(), &BTreeMap::new())["PATH"],
            BASE_PATH
        );
    }
}
