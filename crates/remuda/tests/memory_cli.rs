//! `remuda memory` (D-060) against the in-process Hub, run the way a seat
//! runs it: its own Agent credential and `REMUDA_INSTANCE_ID`, no
//! `--lineage`. The owner passes `--lineage` with a Human token.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use std::io::Write;
use std::process::{Command, Output, Stdio};

const HOST: &str = "hst_memory_cli";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

struct Hub {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    base: String,
    owner: String,
}

async fn spawn_hub() -> Result<Hub> {
    let dir = tempfile::tempdir()?;
    let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
    hub.test_insert_host(HOST).await?;
    let owner = hub.mint_device_token("memory-cli-owner").await?;
    let base = format!("http://{}", hub.addr);
    Ok(Hub {
        _dir: dir,
        hub,
        base,
        owner,
    })
}

impl Hub {
    async fn seat(&self) -> Result<(String, String)> {
        let seat = self
            .hub
            .store()
            .context("store")?
            .insert_instance_delegated(
                HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                serde_json::json!({"origin": "human"}),
                remuda_hub::store_test_support::continuity_delegation(),
                // Hub-internal seeding work, as main's own store-insert tests
                // use: no Agent initiator, never fenced.
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await?
            .instance_id;
        let token = self.hub.test_mint_agent_token("seat", &seat).await?;
        Ok((seat, token))
    }

    fn command(&self, token: &str, instance: Option<&str>) -> Command {
        let mut command = Command::new(bin());
        command
            .arg("memory")
            .env("REMUDA_HUB", &self.base)
            .env("REMUDA_TOKEN", token)
            .env_remove("REMUDA_INSTANCE_ID")
            .env_remove("REMUDA_BOOTSTRAP_TOKEN")
            .env_remove("REMUDA_DATA_DIR")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy");
        if let Some(instance) = instance {
            command.env("REMUDA_INSTANCE_ID", instance);
        }
        command
    }
}

fn run(mut command: Command, args: &[&str]) -> Output {
    command.args(args).output().expect("run remuda memory")
}

fn ok(output: &Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

fn refused(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "expected a refusal; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    stderr
}

#[test]
fn memory_help_lists_the_verbs() {
    let output = Command::new(bin())
        .args(["memory", "--help"])
        .output()
        .unwrap();
    let (stdout, _) = ok(&output);
    for verb in [
        "write",
        "read",
        "search",
        "rm",
        "consolidate",
        "index",
        "restore",
    ] {
        assert!(stdout.contains(verb), "help must list `{verb}`: {stdout}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seat_writes_searches_consolidates_and_removes_its_memory() -> Result<()> {
    let hub = spawn_hub().await?;
    let (seat, token) = hub.seat().await?;
    let source = format!("{seat}:4");
    let as_seat = || hub.command(&token, Some(&seat));

    // Create prints the revision.
    let (stdout, stderr) = ok(&run(
        as_seat(),
        &[
            "write",
            "owner-tone",
            "--type",
            "feedback",
            "--title",
            "Owner wants terse status replies",
            "--description",
            "Lead with the outcome; skip narration",
            "--body",
            "Why: said on 2026-10-29. How to apply: one-line status first.",
            "--source",
            &source,
        ],
    ));
    assert_eq!(stdout, "owner-tone revision 1\n");
    assert_eq!(stderr, "");

    // A second create of the slug is the Hub's 409, not an overwrite.
    let stderr = refused(&run(
        as_seat(),
        &[
            "write",
            "owner-tone",
            "--type",
            "user",
            "--title",
            "t",
            "--description",
            "d",
        ],
    ));
    assert!(stderr.contains("hub HTTP 409"), "{stderr}");
    assert!(stderr.contains("already exists at revision 1"), "{stderr}");

    // Update from stdin at the current revision.
    let mut child = as_seat()
        .args([
            "write",
            "owner-tone",
            "--expected-revision",
            "1",
            "--body-file",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().context("stdin")?.write_all(
        "Why: said on 2026-10-29 (先定边界再动手).\nHow to apply: outcome first.\n".as_bytes(),
    )?;
    let (stdout, _) = ok(&child.wait_with_output()?);
    assert_eq!(stdout, "owner-tone revision 2\n");

    // A stale revision is refused with the current one.
    let stderr = refused(&run(
        as_seat(),
        &[
            "write",
            "owner-tone",
            "--expected-revision",
            "1",
            "--body",
            "late",
        ],
    ));
    assert!(
        stderr.contains("hub HTTP 409: stale memory revision"),
        "{stderr}"
    );
    assert!(stderr.contains("\"currentRevision\": 2"), "{stderr}");

    // Soft limit: written, with the actual numbers.
    let body = "x".repeat(3000);
    let (stdout, stderr) = ok(&run(
        as_seat(),
        &[
            "write",
            "long-ref",
            "--type",
            "reference",
            "--title",
            "Long",
            "--description",
            "Big",
            "--body",
            &body,
        ],
    ));
    assert_eq!(stdout, "long-ref revision 1\n");
    assert_eq!(
        stderr,
        "warning: entry is 3007 bytes (soft limit 2048 bytes)\n"
    );

    // Read shows provenance and the body.
    let (stdout, _) = ok(&run(as_seat(), &["read", "owner-tone"]));
    assert!(stdout.contains("revision: 2\norigin: agent\n"), "{stdout}");
    assert!(stdout.contains(&format!("writer: {seat}\n")), "{stdout}");
    assert!(stdout.contains(&format!("sources: {seat}:4\n")), "{stdout}");
    assert!(
        stdout.ends_with("How to apply: outcome first.\n"),
        "{stdout}"
    );

    // Search: a two-character CJK term by substring, a word by full text.
    let (stdout, _) = ok(&run(as_seat(), &["search", "边界"]));
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stdout.contains("owner-tone") && stdout.ends_with("[like]\n"),
        "{stdout}"
    );
    let (stdout, _) = ok(&run(as_seat(), &["search", "terse"]));
    assert!(
        stdout.contains("owner-tone") && stdout.ends_with("[fts]\n"),
        "{stdout}"
    );

    // Consolidate two entries into one.
    ok(&run(
        as_seat(),
        &[
            "write",
            "owner-tone-2",
            "--type",
            "feedback",
            "--title",
            "No narration",
            "--description",
            "Skip step-by-step narration",
        ],
    ));
    let (stdout, _) = ok(&run(
        as_seat(),
        &[
            "consolidate",
            "--into",
            "owner-tone",
            "--from",
            "owner-tone:2",
            "--from",
            "owner-tone-2:1",
            "--type",
            "feedback",
            "--title",
            "Owner wants terse replies",
            "--description",
            "Outcome first, no narration",
            "--body",
            "Why: said on 2026-10-29. How to apply: outcome line first.",
        ],
    ));
    assert_eq!(stdout, "owner-tone revision 3\nremoved owner-tone-2\n");

    let (stdout, _) = ok(&run(as_seat(), &["index"]));
    assert_eq!(
        stdout,
        "- [feedback] owner-tone: Owner wants terse replies — Outcome first, no narration\n\
         - [reference] long-ref: Long — Big\n"
    );

    let (stdout, _) = ok(&run(
        as_seat(),
        &["rm", "long-ref", "--expected-revision", "1"],
    ));
    assert_eq!(stdout, "deleted long-ref (revision 1)\n");

    // The owner reads with --lineage; the seat cannot restore.
    let (stdout, _) = ok(&run(
        hub.command(&hub.owner, None),
        &["--lineage", &seat, "read"],
    ));
    assert_eq!(
        stdout,
        "- [feedback] owner-tone: Owner wants terse replies — Outcome first, no narration (rev 3)\n"
    );
    let stderr = refused(&run(as_seat(), &["restore"]));
    assert!(stderr.contains("hub HTTP 403"), "{stderr}");
    let (stdout, _) = ok(&run(
        hub.command(&hub.owner, None),
        &["--lineage", &seat, "restore"],
    ));
    assert_eq!(stdout, "restored 0\n");

    // Without --lineage or an instance there is nothing to resolve.
    let stderr = refused(&run(hub.command(&hub.owner, None), &["index"]));
    assert!(stderr.contains("pass --lineage"), "{stderr}");
    Ok(())
}
