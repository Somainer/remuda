//! `wake.check` (cb-wake-check, main-agent.md §11.2): run one wake-up check.
//!
//! A wake-up may carry a shell command that decides whether its run happens.
//! The Hub admitted it (on the OA1 axis) and names this Node; the Node:
//!
//! 1. runs `sh -n -c <command>` first. A syntax error is reported as
//!    `syntax` and nothing executes;
//! 2. runs `sh -c <command>` in its own process group, in the lineage's
//!    workspace (and cwd) on this host, under the mandatory timeout. On the
//!    timeout the whole group is killed. Once the shell exits, whatever it
//!    left running in its group is killed too: a check decides, it does not
//!    start services, and no process of it outlives the report;
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

use crate::{DevNode, NodeError};
use remuda_protocol::{
    METHOD_WAKE_CHECK, WAKE_CHECK_ENV_PREFIX, WAKE_CHECK_STDOUT_MAX_BYTES,
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

/// Run one check: `sh -n`, then the command under `timeout`.
pub(crate) async fn run_check(
    command: &str,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> CheckRun {
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
    let mut sh = shell(cwd, env);
    sh.args(["-c", command]).stdout(Stdio::piped());
    let mut child = match sh.spawn() {
        Ok(child) => child,
        Err(_) => {
            return CheckRun::without_output(WakeCheckOutcome::Unavailable, "could not start sh");
        }
    };
    let pgid = child.id();
    let reader = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(read_bounded(pipe, WAKE_CHECK_STDOUT_MAX_BYTES)));
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    // On the timeout this kills the whole check; after an exit, whatever the
    // shell left behind in its group (which also closes the stdout pipe a
    // straggler may hold).
    kill_group(pgid);
    let timed_out = waited.is_err();
    let status = match waited {
        Ok(status) => status.ok(),
        Err(_) => {
            let _ = child.wait().await;
            None
        }
    };
    let (stdout, cut) = match reader {
        Some(reader) => match tokio::time::timeout(STDOUT_GRACE, reader).await {
            Ok(Ok(read)) => read,
            _ => (Vec::new(), false),
        },
        None => (Vec::new(), false),
    };
    let (stdout, truncated) = bounded_text(&stdout, WAKE_CHECK_STDOUT_MAX_BYTES);
    let stdout_truncated = cut || truncated;
    if timed_out {
        return CheckRun {
            outcome: WakeCheckOutcome::TimedOut,
            exit_code: None,
            signal: None,
            stdout,
            stdout_truncated,
            summary: format!(
                "timed out after {} s; its process group was killed",
                timeout.as_secs()
            ),
        };
    }
    let Some(status) = status else {
        return CheckRun::without_output(WakeCheckOutcome::Unavailable, "its exit was not seen");
    };
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    let exit_code = status.code();
    let summary = match (exit_code, signal) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "ended without an exit code".to_owned(),
    };
    CheckRun {
        outcome: WakeCheckOutcome::Exited,
        exit_code,
        signal,
        stdout,
        stdout_truncated,
        summary,
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
        let workspace = request
            .workspace_id
            .as_deref()
            .map(WorkspaceId::from_str)
            .transpose()?;
        let run = match self.resolve_workspace_cwd(workspace.as_ref(), request.cwd.as_deref()) {
            Ok((_, cwd)) => {
                let env = check_env(&node_env(), &request.env);
                run_check(
                    &request.command,
                    &cwd,
                    &env,
                    Duration::from_secs(request.timeout_secs),
                )
                .await
            }
            Err(_) => CheckRun::without_output(
                WakeCheckOutcome::Unavailable,
                "its working directory is not usable on this Node",
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
        run_check(command, dir, &env(), Duration::from_secs(secs)).await
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
        assert_eq!(missing["outcome"], "unavailable");
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
