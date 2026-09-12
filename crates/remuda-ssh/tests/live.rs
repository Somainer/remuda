//! Live SSH against `devbox-sg`. Ignored by default.
//!
//! Writes only `/tmp/remuda-ssh-test/` on the remote and deletes it afterwards.
//! Prefers `node --stdio` `node.hello`; falls back to `remuda version`.
//!
//! Run: `cargo test -p remuda-ssh --test live -- --ignored --nocapture`

use std::path::PathBuf;
use std::time::Duration;

use remuda_ssh::{
    BootstrapResult, NodeTransport, SshClient, SshOptions, SshTarget, StdioTransport, bootstrap,
    node_stdio_argv, probe,
};

const ALIAS: &str = "devbox-sg";
const REMOTE_DIR: &str = "/tmp/remuda-ssh-test";
const REMOTE_BIN: &str = "/tmp/remuda-ssh-test/remuda";

fn musl_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/x86_64-unknown-linux-musl/release/remuda")
}

struct RemoteCleanup {
    alias: &'static str,
}

impl Drop for RemoteCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ConnectionAttempts=1",
                self.alias,
                "--",
                "rm",
                "-rf",
                REMOTE_DIR,
            ])
            .status();
    }
}

#[tokio::test]
#[ignore = "sshes to devbox-sg once; writes only /tmp/remuda-ssh-test"]
async fn probe_bootstrap_version_on_sg() -> anyhow::Result<()> {
    let local = musl_bin();
    anyhow::ensure!(
        local.is_file(),
        "missing musl remuda at {} (run `just linux-musl`)",
        local.display()
    );

    let _cleanup = RemoteCleanup { alias: ALIAS };
    let mut options = SshOptions::with_control_master();
    options.connect_timeout_secs = Some(8);
    let client = SshClient::new(ALIAS, options);

    let target = SshTarget::resolve(ALIAS)?;
    anyhow::ensure!(target.alias == ALIAS);
    anyhow::ensure!(target.port > 0);

    let report = probe(&client, target).await?;
    println!("{}", report.display_text());
    anyhow::ensure!(!report.uname.is_empty(), "uname");
    anyhow::ensure!(!report.glibc.is_empty(), "glibc");

    let result = bootstrap(&client, &local, REMOTE_BIN).await?;
    match &result {
        BootstrapResult::Uploaded {
            digest,
            remote_path,
            version,
        }
        | BootstrapResult::Skipped {
            digest,
            remote_path,
            version,
        } => {
            println!("bootstrap digest={digest} path={remote_path}");
            print!("{version}");
            anyhow::ensure!(remote_path == REMOTE_BIN);
            anyhow::ensure!(digest.starts_with("sha256:"));
            anyhow::ensure!(version.contains("remuda "), "version stdout: {version}");
        }
    }

    let argv = node_stdio_argv(std::path::Path::new(REMOTE_BIN));
    let hello = tokio::time::timeout(Duration::from_secs(8), async {
        let mut t = StdioTransport::connect_ssh(client.clone(), argv).await?;
        let frame = t.recv_json().await?;
        let _ = t.close().await;
        anyhow::Ok(frame)
    })
    .await;

    match hello {
        Ok(Ok(Some(frame)))
            if frame.get("method").and_then(|m| m.as_str()) == Some("node.hello")
                || frame.get("jsonrpc").is_some() =>
        {
            println!("{frame}");
        }
        other => {
            println!("node --stdio hello unavailable: {other:?}");
            let version = client
                .exec(&[REMOTE_BIN, "version"], None, Duration::from_secs(15))
                .await?
                .ok()?;
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "carrier": "ssh-stdio",
                    "mode": "version-fallback",
                    "reason": "remote remuda did not emit node.hello on --stdio",
                    "alias": ALIAS,
                    "version": version.stdout,
                })
            );
        }
    }

    client.control_exit().await?;
    Ok(())
}
