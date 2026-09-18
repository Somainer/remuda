//! Spawn `claude -p` stream-json and run the control handshake.

use crate::codec::{self, DEFAULT_MAX_LINE_BYTES};
use crate::error::Error;
use crate::types::{
    ControlRequest, ControlRequestEnvelope, ControlSuccessPayload, Inbound, InitializeRequest,
    Outbound, PermissionMode, PermissionResult, UserContent, UserMessage,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Settings overlay: a file path or inline JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsArg {
    /// `--settings /path/to/settings.json`.
    Path(PathBuf),
    /// `--settings '{"model":"..."}'`.
    Json(String),
}

/// How `--setting-sources` is passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingSources {
    /// Omit the flag (CLI default sources).
    Omit,
    /// Pass the given string, including `""` to disable user/project/local.
    Value(String),
}

/// Which argv template [`SpawnSpec::argv`] builds.
///
/// The two carriers differ by exactly one flag. `-p` is "Print response and
/// exit", which ends the child after one turn and is the whole reason
/// `claude-print` cannot be a worker carrier (`print-replacement.md` §2.1,
/// D-035). Non-interactive mode does not need it: the CLI treats a session as
/// non-interactive when stdout is not a TTY, and the SDK child has piped stdout,
/// so the Agent SDK's own argv builder omits `-p` too (§1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpawnMode {
    /// `claude -p` — one response, then the process exits. Default for
    /// compatibility with the existing `claude-print` carrier.
    #[default]
    Print,
    /// Agent-SDK-shaped stream-json with **no** `-p`, so stdin stays open and
    /// each further `user` line is another turn on the same child (§2.1).
    Sdk,
}

/// Spawn configuration for a Claude print session.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Claude binary. Default `claude`.
    pub binary: PathBuf,
    /// Which argv template to build. Default [`SpawnMode::Print`].
    pub mode: SpawnMode,
    /// Working directory (`Command::current_dir`; never `--cwd`).
    pub cwd: PathBuf,
    /// `--session-id`. Generated when `None` and `resume` is `None`.
    pub session_id: Option<String>,
    /// `--resume <uuid>` (mutually exclusive with `session_id`).
    pub resume: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--permission-mode`. Default `default`.
    pub permission_mode: PermissionMode,
    /// `--setting-sources`. Default `user,project,local`.
    pub setting_sources: SettingSources,
    /// `--settings` file or inline JSON.
    pub settings: Option<SettingsArg>,
    /// Extra environment overlays.
    pub env: BTreeMap<String, String>,
    /// Extra argv appended after the standard template.
    pub extra_args: Vec<String>,
    /// When set, this argv is used instead of the print template (tests).
    pub raw_argv: Option<Vec<String>>,
    /// `--include-partial-messages`. Default true.
    pub include_partial_messages: bool,
    /// `--include-hook-events`. Default true (vibe-kanban does not set this).
    pub include_hook_events: bool,
    /// `--forward-subagent-text`. Default true (vibe-kanban does not set this).
    pub forward_subagent_text: bool,
    /// `--replay-user-messages`. Default true.
    pub replay_user_messages: bool,
    /// `--allow-dangerously-skip-permissions`.
    pub allow_dangerously_skip_permissions: bool,
    /// `--max-budget-usd`.
    pub max_budget_usd: Option<f64>,
    /// `--effort`.
    pub effort: Option<String>,
    /// `--verbose`. Default true (required for a full stream-json event set).
    pub verbose: bool,
    /// Body of the automatic `initialize` control request.
    pub initialize: InitializeRequest,
    /// How long to wait for the initialize `control_response`.
    pub handshake_timeout: Duration,
    /// NDJSON line limit.
    pub max_line_bytes: usize,
}

impl Default for SpawnSpec {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            mode: SpawnMode::Print,
            cwd: PathBuf::from("."),
            session_id: None,
            resume: None,
            model: None,
            permission_mode: PermissionMode::Default,
            setting_sources: SettingSources::Value("user,project,local".into()),
            settings: None,
            env: BTreeMap::new(),
            extra_args: Vec::new(),
            raw_argv: None,
            include_partial_messages: true,
            include_hook_events: true,
            forward_subagent_text: true,
            replay_user_messages: true,
            allow_dangerously_skip_permissions: false,
            max_budget_usd: None,
            effort: None,
            verbose: true,
            initialize: InitializeRequest::default(),
            handshake_timeout: Duration::from_secs(30),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

impl SpawnSpec {
    /// Stream-json argv (never `--bare`, `--no-session-persistence`, `--cwd`, or a prompt).
    ///
    /// [`SpawnMode::Print`] leads with `-p`; [`SpawnMode::Sdk`] omits it and is
    /// otherwise identical, so the two carriers cannot drift apart.
    pub fn argv(&self) -> Result<Vec<OsString>, Error> {
        if let Some(raw) = &self.raw_argv {
            check_forbidden(raw.iter().map(String::as_str))?;
            return Ok(raw.iter().map(OsString::from).collect());
        }
        let mut argv: Vec<OsString> = Vec::new();
        if self.mode == SpawnMode::Print {
            argv.push("-p".into());
        }
        argv.extend::<Vec<OsString>>(vec![
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--permission-prompts".into(),
            "host".into(),
            "--permission-prompt-tool".into(),
            "stdio".into(),
        ]);
        if self.verbose {
            argv.push("--verbose".into());
        }
        if self.include_partial_messages {
            argv.push("--include-partial-messages".into());
        }
        if self.include_hook_events {
            argv.push("--include-hook-events".into());
        }
        if self.forward_subagent_text {
            argv.push("--forward-subagent-text".into());
        }
        if self.replay_user_messages {
            argv.push("--replay-user-messages".into());
        }
        argv.push("--permission-mode".into());
        argv.push(self.permission_mode.as_cli_str()?.into());
        if self.allow_dangerously_skip_permissions {
            argv.push("--allow-dangerously-skip-permissions".into());
        }
        match &self.setting_sources {
            SettingSources::Omit => {}
            SettingSources::Value(value) => {
                argv.push("--setting-sources".into());
                argv.push(value.into());
            }
        }
        if let Some(settings) = &self.settings {
            argv.push("--settings".into());
            match settings {
                SettingsArg::Path(path) => argv.push(path.into()),
                SettingsArg::Json(json) => argv.push(json.into()),
            }
        }
        if let Some(model) = &self.model {
            argv.push("--model".into());
            argv.push(model.into());
        }
        if let Some(effort) = &self.effort {
            argv.push("--effort".into());
            argv.push(effort.into());
        }
        if let Some(budget) = self.max_budget_usd {
            argv.push("--max-budget-usd".into());
            argv.push(budget.to_string().into());
        }
        if let Some(resume) = &self.resume {
            argv.push("--resume".into());
            argv.push(resume.into());
        } else {
            let session_id = self
                .session_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            argv.push("--session-id".into());
            argv.push(session_id.into());
        }
        check_forbidden(self.extra_args.iter().map(String::as_str))?;
        for extra in &self.extra_args {
            argv.push(extra.into());
        }
        check_forbidden(argv.iter().filter_map(|a| a.to_str()))?;
        Ok(argv)
    }

    fn command(&self) -> Result<Command, Error> {
        let argv = self.argv()?;
        let mut command = Command::new(&self.binary);
        command
            .args(&argv)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        Ok(command)
    }
}

fn check_forbidden<'a, I>(args: I) -> Result<(), Error>
where
    I: IntoIterator<Item = &'a str>,
{
    for arg in args {
        let name = arg.split('=').next().unwrap_or(arg);
        if name == "--bare"
            || name == "--no-session-persistence"
            || name == "--cwd"
            || name == "--bg"
        {
            return Err(Error::forbidden(arg));
        }
    }
    Ok(())
}

enum WriterCmd {
    Message(Box<Inbound>),
    Close,
}

/// Live Claude print process plus helpers that write stdin.
pub struct ClaudeProcess {
    inbound: mpsc::Sender<Inbound>,
    writer: mpsc::Sender<WriterCmd>,
    child: Child,
    reader: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    inbound_forward: JoinHandle<()>,
    replay: JoinHandle<()>,
}

impl ClaudeProcess {
    /// Spawn `claude` with the print template, send `initialize`, wait for its
    /// `control_response`, and keep forwarding every other stdout frame
    /// (including `system/init` that may arrive first, and later `result`s).
    pub async fn spawn(
        spec: SpawnSpec,
    ) -> Result<(mpsc::Sender<Inbound>, mpsc::Receiver<Outbound>, Self), Error> {
        let command = spec.command()?;
        Self::spawn_command(
            command,
            &spec.binary,
            spec.initialize,
            spec.handshake_timeout,
            spec.max_line_bytes,
        )
        .await
    }

    /// Spawn an already-built command (used by tests with a fake peer).
    pub async fn spawn_command(
        mut command: Command,
        binary: &Path,
        initialize: InitializeRequest,
        handshake_timeout: Duration,
        max_line_bytes: usize,
    ) -> Result<(mpsc::Sender<Inbound>, mpsc::Receiver<Outbound>, Self), Error> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| Error::Spawn {
            binary: binary.to_path_buf(),
            source,
        })?;
        let stdin = child.stdin.take().ok_or(Error::MissingPipe("stdin"))?;
        let stdout = child.stdout.take().ok_or(Error::MissingPipe("stdout"))?;
        let stderr = child.stderr.take().ok_or(Error::MissingPipe("stderr"))?;

        let (pub_in_tx, mut pub_in_rx) = mpsc::channel::<Inbound>(64);
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterCmd>(64);
        let (raw_out_tx, mut raw_out_rx) = mpsc::channel::<Outbound>(256);
        let (pub_out_tx, pub_out_rx) = mpsc::channel::<Outbound>(256);

        let writer_task = {
            let mut stdin = stdin;
            tokio::spawn(async move {
                while let Some(cmd) = writer_rx.recv().await {
                    match cmd {
                        WriterCmd::Message(msg) => {
                            if let Err(error) = codec::write_line(&mut stdin, msg.as_ref()).await {
                                warn!(%error, "claude stdin write failed");
                                break;
                            }
                        }
                        WriterCmd::Close => {
                            if let Err(error) = tokio::io::AsyncWriteExt::shutdown(&mut stdin).await
                            {
                                debug!(%error, "claude stdin shutdown");
                            }
                            break;
                        }
                    }
                }
            })
        };

        let inbound_forward = {
            let writer_tx = writer_tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = pub_in_rx.recv().await {
                    if writer_tx
                        .send(WriterCmd::Message(Box::new(msg)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match codec::read_outbound(&mut reader, max_line_bytes).await {
                    Ok(Some(msg)) => {
                        if raw_out_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        warn!(%error, "claude stdout read failed");
                        break;
                    }
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        debug!(target: "remuda_claude_wire::stderr", "{line}");
                    }
                    Ok(None) => break,
                    Err(error) => {
                        debug!(%error, "claude stderr closed");
                        break;
                    }
                }
            }
        });

        let (init_id, init_msg) = Inbound::initialize(initialize);
        writer_tx
            .send(WriterCmd::Message(Box::new(init_msg)))
            .await
            .map_err(|_| Error::StdinClosed)?;

        let handshake = async {
            let mut buffered = Vec::new();
            loop {
                let msg = raw_out_rx.recv().await.ok_or_else(|| Error::HandshakeEof {
                    request_id: init_id.clone(),
                })?;
                let done = match &msg {
                    Outbound::ControlResponse(env) if env.response.request_id() == init_id => {
                        match &env.response {
                            crate::types::ControlResponse::Success { .. } => Some(Ok(())),
                            crate::types::ControlResponse::Error { error, .. } => {
                                Some(Err(Error::HandshakeFailed {
                                    request_id: init_id.clone(),
                                    message: error.clone().unwrap_or_else(|| "error".into()),
                                }))
                            }
                        }
                    }
                    _ => None,
                };
                buffered.push(msg);
                if let Some(result) = done {
                    result?;
                    break;
                }
            }
            Ok::<Vec<Outbound>, Error>(buffered)
        };

        let buffered = match tokio::time::timeout(handshake_timeout, handshake).await {
            Ok(Ok(buffered)) => buffered,
            Ok(Err(error)) => {
                let _ = child.start_kill();
                return Err(error);
            }
            Err(_) => {
                let _ = child.start_kill();
                return Err(Error::HandshakeTimeout {
                    request_id: init_id,
                    timeout_ms: u64::try_from(handshake_timeout.as_millis()).unwrap_or(u64::MAX),
                });
            }
        };

        let replay = tokio::spawn(async move {
            for msg in buffered {
                if pub_out_tx.send(msg).await.is_err() {
                    return;
                }
            }
            while let Some(msg) = raw_out_rx.recv().await {
                if pub_out_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let process = Self {
            inbound: pub_in_tx.clone(),
            writer: writer_tx,
            child,
            reader,
            writer_task,
            stderr_task,
            inbound_forward,
            replay,
        };
        Ok((pub_in_tx, pub_out_rx, process))
    }

    /// Channel used by [`Self::send_user`] / [`Self::respond_control`].
    pub fn inbound(&self) -> &mpsc::Sender<Inbound> {
        &self.inbound
    }

    /// Child pid, if still known.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Send a user turn (`content` string or blocks).
    pub async fn send_user(&self, content: UserContent) -> Result<(), Error> {
        let message = match content {
            UserContent::Text(text) => UserMessage::text(text),
            UserContent::Blocks(blocks) => UserMessage::blocks(blocks),
        };
        self.send(Inbound::User(message)).await
    }

    /// Answer a CLI control request with a success payload (permission camelCase).
    pub async fn respond_control(
        &self,
        request_id: impl Into<String>,
        result: ControlSuccessPayload,
    ) -> Result<(), Error> {
        self.send(Inbound::control_success(request_id, result))
            .await
    }

    /// Allow a `can_use_tool` request, echoing `updatedInput`.
    pub async fn allow_tool(
        &self,
        request_id: impl Into<String>,
        updated_input: serde_json::Value,
    ) -> Result<(), Error> {
        self.respond_control(
            request_id,
            ControlSuccessPayload::Permission(PermissionResult::Allow {
                updated_input,
                updated_permissions: None,
            }),
        )
        .await
    }

    /// Send `interrupt`. Returns the new `request_id`.
    pub async fn interrupt(&self, cancel_queued: bool) -> Result<String, Error> {
        self.send_control(ControlRequest::Interrupt {
            cancel_queued: Some(cancel_queued),
        })
        .await
    }

    /// Send `set_permission_mode`. Returns the new `request_id`.
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<String, Error> {
        let _ = mode.as_cli_str()?;
        self.send_control(ControlRequest::SetPermissionMode { mode })
            .await
    }

    /// Send `set_model`. Returns the new `request_id`.
    pub async fn set_model(&self, model: Option<String>) -> Result<String, Error> {
        self.send_control(ControlRequest::SetModel { model }).await
    }

    /// Close Claude stdin. The process exits after in-flight work (and
    /// background Workflow/subagent wait) once no more turns can be sent.
    pub async fn close_stdin(&self) -> Result<(), Error> {
        self.writer
            .send(WriterCmd::Close)
            .await
            .map_err(|_| Error::StdinClosed)
    }

    /// SIGKILL the child. SIGTERM is **not** interrupt (exit 143, no `result`).
    pub fn kill(&mut self) -> Result<(), Error> {
        self.child.start_kill().map_err(Error::from)
    }

    /// Wait for the child to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, Error> {
        Ok(self.child.wait().await?)
    }

    async fn send(&self, msg: Inbound) -> Result<(), Error> {
        self.writer
            .send(WriterCmd::Message(Box::new(msg)))
            .await
            .map_err(|_| Error::StdinClosed)
    }

    async fn send_control(&self, request: ControlRequest) -> Result<String, Error> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.send(Inbound::ControlRequest(ControlRequestEnvelope {
            request_id: request_id.clone(),
            request,
        }))
        .await?;
        Ok(request_id)
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer_task.abort();
        self.stderr_task.abort();
        self.inbound_forward.abort();
        self.replay.abort();
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_never_includes_forbidden_flags() {
        let spec = SpawnSpec {
            model: Some("haiku".into()),
            session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            ..SpawnSpec::default()
        };
        let argv = spec.argv().expect("argv");
        let text: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.contains(&"-p".into()));
        assert!(text.contains(&"--permission-prompts".into()));
        assert!(text.contains(&"host".into()));
        assert!(text.contains(&"--permission-prompt-tool".into()));
        assert!(text.contains(&"stdio".into()));
        assert!(text.contains(&"--include-hook-events".into()));
        assert!(text.contains(&"--forward-subagent-text".into()));
        assert!(!text.iter().any(|a| a.starts_with("--bare")));
        assert!(!text.iter().any(|a| a.contains("session-persistence")));
        assert!(!text.iter().any(|a| a == "--cwd"));
        assert!(!text.iter().any(|a| a == "--bg"));
        assert_eq!(text.iter().filter(|a| a.starts_with("Review")).count(), 0);
    }

    #[test]
    fn argv_rejects_bare() {
        let spec = SpawnSpec {
            extra_args: vec!["--bare".into()],
            ..SpawnSpec::default()
        };
        let err = spec.argv().expect_err("bare");
        assert!(matches!(err, Error::ForbiddenFlag { .. }));
    }

    fn argv_text(spec: &SpawnSpec) -> Vec<String> {
        spec.argv()
            .expect("argv")
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The whole point of the sdk carrier: `-p` is "Print response and exit",
    /// so it is the one flag that must not appear (`print-replacement.md` §2.1,
    /// key decision 3). Everything else stays byte-identical to print, which is
    /// what keeps the two templates from drifting.
    #[test]
    fn sdk_mode_drops_dash_p_and_changes_nothing_else() {
        let base = SpawnSpec {
            model: Some("haiku".into()),
            session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            ..SpawnSpec::default()
        };
        let print = argv_text(&base);
        let sdk = argv_text(&SpawnSpec {
            mode: SpawnMode::Sdk,
            ..base.clone()
        });

        assert_eq!(print.first().map(String::as_str), Some("-p"));
        assert!(!sdk.iter().any(|a| a == "-p" || a == "--print"));
        assert_eq!(sdk, print[1..].to_vec());

        // Non-interactive still comes from piped stdout, so the stream-json
        // contract and the host permission channel are unchanged (§1.3, §4.2.3).
        for flag in [
            "--input-format",
            "--output-format",
            "stream-json",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            "stdio",
            "--include-partial-messages",
            "--include-hook-events",
            "--forward-subagent-text",
            "--replay-user-messages",
        ] {
            assert!(sdk.iter().any(|a| a == flag), "sdk argv missing {flag}");
        }
        // §2.1: never these, on either template.
        for flag in [
            "--bare",
            "--safe-mode",
            "--no-session-persistence",
            "--continue",
        ] {
            assert!(!sdk.iter().any(|a| a == flag), "sdk argv has {flag}");
        }
    }

    /// Resume is a new process continuing the same native JSONL (D-026), and it
    /// is `--resume <id>` rather than `--continue` on both templates.
    #[test]
    fn sdk_mode_resume_passes_the_session_id() {
        let sdk = argv_text(&SpawnSpec {
            mode: SpawnMode::Sdk,
            resume: Some("22222222-2222-4222-8222-222222222222".into()),
            session_id: None,
            ..SpawnSpec::default()
        });
        let at = sdk.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(
            sdk.get(at + 1).map(String::as_str),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert!(!sdk.iter().any(|a| a == "--session-id"));
        assert!(!sdk.iter().any(|a| a == "-p"));
    }
}
