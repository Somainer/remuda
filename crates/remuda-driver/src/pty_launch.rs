//! Wait for the shell's foreground job before dispatching a Herdr agent.

use async_trait::async_trait;
use remuda_herdr::{
    AgentStartParams, AgentStarted, Client, Error, PaneProcessInfo, PaneReadParams, ReadFormat,
    ReadSource,
};
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};

const READY_WAIT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(200);
const READ_WAIT: Duration = Duration::from_secs(2);
const SCREEN_LINES: usize = 32;
const SCREEN_BYTES: usize = 4096;

#[async_trait]
trait LaunchClient: Sync {
    async fn processes(&self, pane_id: &str) -> Result<Option<PaneProcessInfo>, Error>;
    async fn start(&self, params: AgentStartParams) -> Result<AgentStarted, Error>;
    async fn screen(&self, pane_id: &str) -> Result<String, Error>;
}

#[async_trait]
impl LaunchClient for Client {
    async fn processes(&self, pane_id: &str) -> Result<Option<PaneProcessInfo>, Error> {
        Ok(self
            .pane_process_info(Some(pane_id.into()))
            .await?
            .process_info)
    }

    async fn start(&self, params: AgentStartParams) -> Result<AgentStarted, Error> {
        self.agent_start(params).await
    }

    async fn screen(&self, pane_id: &str) -> Result<String, Error> {
        Ok(self
            .pane_read(PaneReadParams {
                pane_id: pane_id.into(),
                source: ReadSource::RecentUnwrapped,
                lines: Some(SCREEN_LINES as u32),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await?
            .text()
            .to_owned())
    }
}

pub(crate) async fn start_agent(
    client: &Client,
    params: AgentStartParams,
) -> Result<AgentStarted, Error> {
    start_when_ready(client, params, READY_WAIT, READY_POLL).await
}

async fn start_when_ready(
    client: &impl LaunchClient,
    params: AgentStartParams,
    wait: Duration,
    poll: Duration,
) -> Result<AgentStarted, Error> {
    let deadline = Instant::now() + wait;
    let mut last_state = "shell readiness not observed".to_owned();
    while Instant::now() < deadline {
        let read_deadline = deadline.min(Instant::now() + READ_WAIT);
        match timeout_at(read_deadline, client.processes(&params.pane_id)).await {
            Ok(Ok(Some(process))) if available_shell(&process, &params.pane_id) => {
                // Busy is Herdr's pre-dispatch rejection. Other failures may
                // follow dispatch, so preserve the native startup RPC timeout
                // and never replay on timeout, disconnect or agent_not_ready.
                if Instant::now() >= deadline {
                    break;
                }
                match client.start(params.clone()).await {
                    Err(Error::Api { code, message, .. }) if code == "agent_pane_busy" => {
                        last_state = format!("agent_pane_busy: {}", screen_tail(&message));
                    }
                    result => return result,
                }
            }
            Ok(Ok(Some(process))) => {
                last_state = format!(
                    "shell_pid={:?}, foreground_process_group_id={:?}, foreground_processes={}",
                    process.shell_pid,
                    process.foreground_process_group_id,
                    process.foreground_processes.len()
                );
            }
            Ok(Ok(None)) => last_state = "pane process metadata missing".into(),
            Ok(Err(error)) => return Err(error),
            Err(_) => last_state = "pane.process_info timed out".into(),
        }
        sleep_until(deadline.min(Instant::now() + poll)).await;
    }

    // The final diagnostic has a separate two-second bound. Also cap the
    // returned text locally in case the peer ignores its requested line limit.
    let screen = match timeout(READ_WAIT, client.screen(&params.pane_id)).await {
        Ok(Ok(screen)) => screen_tail(&screen),
        Ok(Err(error)) => format!("<screen unavailable: {}>", screen_tail(&error.to_string())),
        Err(_) => "<screen read timed out>".into(),
    };
    Err(Error::Api {
        method: "agent.start".into(),
        code: "agent_pane_busy".into(),
        message: format!(
            "pane {} did not become an available shell within {wait:?}; last state: {last_state}; last screen lines:\n{screen}",
            params.pane_id
        ),
    })
}

fn available_shell(process: &PaneProcessInfo, pane_id: &str) -> bool {
    let Some(shell_pid) = process.shell_pid.filter(|pid| *pid != 0) else {
        return false;
    };
    // These process facts implement Herdr's available-shell contract; prompt
    // characters and agent_status are not evidence of shell job readiness.
    if process.pane_id != pane_id || process.foreground_process_group_id != Some(shell_pid) {
        return false;
    }
    let [shell] = process.foreground_processes.as_slice() else {
        return false;
    };
    if shell.pid != shell_pid {
        return false;
    }
    let basename = shell.name.split(['/', '\\']).next_back().unwrap_or("");
    let normalized = basename
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    const SHELL_NAMES: &[&str] = &[
        "bash",
        "csh",
        "dash",
        "elvish",
        "fish",
        "ksh",
        "mksh",
        "nu",
        "sh",
        "tcsh",
        "xonsh",
        "zsh",
        "cmd",
        "powershell",
        "pwsh",
    ];
    SHELL_NAMES.contains(&normalized.as_str())
}

fn screen_tail(screen: &str) -> String {
    let mut lines: Vec<_> = screen.lines().rev().take(SCREEN_LINES).collect();
    lines.reverse();
    let tail = lines.join("\n");
    let mut start = tail.len().saturating_sub(SCREEN_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    tail[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remuda_herdr::PaneProcessInfoProcess;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeClient {
        processes: Mutex<VecDeque<PaneProcessInfo>>,
        starts: Mutex<VecDeque<Result<AgentStarted, Error>>>,
        reads: AtomicUsize,
        writes: AtomicUsize,
        screen: String,
        hang_reads: bool,
    }

    impl FakeClient {
        fn new(processes: Vec<PaneProcessInfo>, starts: Vec<Result<AgentStarted, Error>>) -> Self {
            Self {
                processes: Mutex::new(processes.into()),
                starts: Mutex::new(starts.into()),
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
                screen: "shell startup still running".into(),
                hang_reads: false,
            }
        }
    }

    #[async_trait]
    impl LaunchClient for FakeClient {
        async fn processes(&self, _: &str) -> Result<Option<PaneProcessInfo>, Error> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.hang_reads {
                std::future::pending::<()>().await;
            }
            let mut processes = self.processes.lock().unwrap();
            Ok(Some(if processes.len() > 1 {
                processes.pop_front().unwrap()
            } else {
                processes.front().unwrap().clone()
            }))
        }

        async fn start(&self, _: AgentStartParams) -> Result<AgentStarted, Error> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            self.starts
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected launch replay")
        }

        async fn screen(&self, _: &str) -> Result<String, Error> {
            if self.hang_reads {
                std::future::pending::<()>().await;
            }
            Ok(self.screen.clone())
        }
    }

    fn process(ready: bool) -> PaneProcessInfo {
        PaneProcessInfo {
            pane_id: "w1:p2".into(),
            shell_pid: Some(42),
            foreground_process_group_id: Some(if ready { 42 } else { 43 }),
            foreground_processes: vec![PaneProcessInfoProcess {
                pid: if ready { 42 } else { 43 },
                name: if ready { "zsh" } else { "sleep" }.into(),
                argv0: None,
                argv: None,
            }],
        }
    }

    fn started() -> Result<AgentStarted, Error> {
        Ok(serde_json::from_value(serde_json::json!({
            "type": "agent_started", "agent": {"pane_id": "w1:p2"}
        }))
        .unwrap())
    }

    fn api_error(code: &str) -> Error {
        Error::Api {
            method: "agent.start".into(),
            code: code.into(),
            message: "test error".into(),
        }
    }

    async fn launch(client: &FakeClient) -> Result<AgentStarted, Error> {
        start_when_ready(
            client,
            AgentStartParams {
                name: "test-agent".into(),
                kind: "claude".into(),
                pane_id: "w1:p2".into(),
                args: vec![],
                timeout_ms: None,
            },
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
    }

    #[tokio::test]
    async fn busy_then_available_waits_before_start() {
        let client = FakeClient::new(vec![process(false), process(true)], vec![started()]);
        launch(&client).await.unwrap();
        assert_eq!(client.reads.load(Ordering::SeqCst), 2);
        assert_eq!(client.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn always_busy_fails_with_last_screen_without_starting() {
        let mut client = FakeClient::new(vec![process(false)], vec![]);
        client.screen = format!("old line\n{}\nlast screen line", "界".repeat(2000));
        let error = launch(&client).await.unwrap_err().to_string();
        assert!(error.contains("did not become an available shell"));
        assert!(error.contains("last screen line"));
        assert!(!error.contains("old line"));
        assert_eq!(client.writes.load(Ordering::SeqCst), 0);
        assert!(error.len() < SCREEN_BYTES + 512);
    }

    #[tokio::test]
    async fn hanging_readiness_and_screen_rpcs_are_bounded() {
        let mut client = FakeClient::new(vec![], vec![]);
        client.hang_reads = true;
        let error = timeout(Duration::from_secs(5), launch(&client))
            .await
            .expect("launch readiness and diagnostic reads must be bounded")
            .unwrap_err()
            .to_string();
        assert!(error.contains("pane.process_info timed out"));
        assert!(error.contains("screen read timed out"));
        assert_eq!(client.writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn definite_busy_after_ready_rechecks_and_retries() {
        let client = FakeClient::new(
            vec![process(true), process(false), process(true)],
            vec![Err(api_error("agent_pane_busy")), started()],
        );
        launch(&client).await.unwrap();
        assert_eq!(client.reads.load(Ordering::SeqCst), 3);
        assert_eq!(client.writes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn other_start_errors_never_replay() {
        for error in [
            api_error("agent_not_ready"),
            api_error("agent_blocked"),
            Error::Timeout {
                method: "agent.start".into(),
                timeout: Duration::from_secs(1),
            },
            Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        ] {
            let client = FakeClient::new(vec![process(true)], vec![Err(error)]);
            assert!(launch(&client).await.is_err());
            assert_eq!(client.writes.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn incomplete_or_foreground_command_metadata_is_not_ready() {
        let ready = process(true);
        assert!(available_shell(&ready, "w1:p2"));
        let mut missing_group = ready.clone();
        missing_group.foreground_process_group_id = None;
        let mut no_shell = ready.clone();
        no_shell.foreground_processes.clear();
        let mut foreground_child = ready.clone();
        foreground_child
            .foreground_processes
            .push(process(false).foreground_processes.remove(0));
        let mut wrong_process = ready.clone();
        wrong_process.foreground_processes[0].name = "claude".into();
        for process in [missing_group, no_shell, foreground_child, wrong_process] {
            assert!(!available_shell(&process, "w1:p2"));
        }
        assert!(!available_shell(&ready, "w1:p3"));
    }

    #[test]
    fn screen_diagnostic_keeps_only_last_lines() {
        let text = (0..40).map(|n| format!("line {n}\n")).collect::<String>();
        let tail = screen_tail(&text);
        assert_eq!(tail.lines().count(), SCREEN_LINES);
        assert!(tail.starts_with("line 8\n"));
        assert!(tail.ends_with("line 39"));
    }
}
