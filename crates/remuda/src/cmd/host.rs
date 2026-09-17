//! `remuda host files ls|get` — read-only workstation file access.
//!
//! Thin operator CLI over the Hub host-file routes. The Hub proxies to the
//! addressed Node, which confines every path to a registered workspace root
//! or the `/tmp/remuda-*` scratch area; this command adds no filesystem logic.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on};

/// `remuda host` subcommands.
#[derive(clap::Args)]
#[command(about = "Inspect files on an enrolled host (read-only).")]
pub(crate) struct Args {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: HostCommand,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

#[derive(Subcommand)]
enum HostCommand {
    /// Workspace file access.
    Files {
        #[command(subcommand)]
        command: FilesCommand,
    },
}

#[derive(Subcommand)]
enum FilesCommand {
    /// List a workspace directory on a host (`hst_…` id or host label).
    #[command(visible_alias = "ls")]
    List {
        /// Host id (`hst_…`) or registered label.
        host: String,
        /// Workspace id (`wsp_…`), or `tmp` for the /tmp/remuda-* area.
        workspace: String,
        /// Workspace-relative subdirectory; defaults to the workspace root.
        #[arg(default_value = "")]
        subpath: String,
    },
    /// Fetch one regular file; writes to `-o out` or stdout.
    Get {
        /// Host id (`hst_…`) or registered label.
        host: String,
        /// Workspace id (`wsp_…`), or `tmp` for the /tmp/remuda-* area.
        workspace: String,
        /// Workspace-relative file path.
        path: String,
        /// Output file; omit to stream bytes to stdout.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,
    },
}

fn run(hub: HubOpts, command: HostCommand) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        match command {
            HostCommand::Files { command } => match command {
                FilesCommand::List {
                    host,
                    workspace,
                    subpath,
                } => list(&client, &host, &workspace, &subpath).await,
                FilesCommand::Get {
                    host,
                    workspace,
                    path,
                    output,
                } => get(&client, &host, &workspace, &path, output).await,
            },
        }
    })
}

/// Resolve a host id or label to the `hst_…` id used in routes.
async fn resolve_host(client: &HubClient, host: &str) -> Result<String> {
    if host.starts_with("hst_") {
        return Ok(host.to_owned());
    }
    let hosts = client.list_hosts().await?;
    let matches = hosts
        .iter()
        .filter(|row| {
            row.get("id").and_then(Value::as_str) == Some(host)
                || row.get("hostId").and_then(Value::as_str) == Some(host)
                || row.get("label").and_then(Value::as_str) == Some(host)
                || row.get("name").and_then(Value::as_str) == Some(host)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [row] => row
            .get("hostId")
            .or_else(|| row.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("host record has no id"),
        [] => bail!("no online or known host matches {host}"),
        _ => bail!("host label {host} is ambiguous; use the hst_ id"),
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte.into())
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

async fn list(client: &HubClient, host: &str, workspace: &str, subpath: &str) -> Result<()> {
    let host_id = resolve_host(client, host).await?;
    let mut path = format!(
        "/v1/hosts/{host_id}/files?workspaceId={}",
        urlencode(workspace)
    );
    if !subpath.trim().is_empty() {
        path.push_str("&relPath=");
        path.push_str(&urlencode(subpath));
    }
    let value = client.get(&path).await?;
    let Some(entries) = value.get("entries").and_then(Value::as_array) else {
        bail!("unexpected host files response: {value}");
    };
    if let Some(root) = value.get("path").and_then(Value::as_str) {
        println!("{root}");
    }
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        rows.push(vec![
            entry["name"].as_str().unwrap_or("").to_owned(),
            entry["kind"].as_str().unwrap_or("").to_owned(),
            entry["size"].as_u64().unwrap_or(0).to_string(),
            format_mode(entry["mode"].as_u64().unwrap_or(0)),
            format_mtime(entry["mtime"].as_u64().unwrap_or(0)),
        ]);
    }
    print!(
        "{}",
        super::table::render(
            &["NAME", "KIND", "SIZE", "MODE", "MODIFIED"],
            &[36, 8, 10, 6, 24],
            &rows
        )
    );
    Ok(())
}

#[cfg(unix)]
fn format_mode(mode: u64) -> String {
    let mode = u32::try_from(mode).unwrap_or(0);
    fn triplet(bits: u32) -> String {
        let mut out = String::new();
        out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
        out
    }
    let mut out = String::new();
    out.push_str(&triplet((mode >> 6) & 0o7));
    out.push_str(&triplet((mode >> 3) & 0o7));
    out.push_str(&triplet(mode & 0o7));
    out
}

#[cfg(not(unix))]
fn format_mode(_mode: u64) -> String {
    "-".into()
}

fn format_mtime(epoch_secs: u64) -> String {
    i64::try_from(epoch_secs)
        .ok()
        .and_then(|secs| time::OffsetDateTime::from_unix_timestamp(secs).ok())
        .map(|stamp| {
            stamp
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

async fn get(
    client: &HubClient,
    host: &str,
    workspace: &str,
    file_path: &str,
    output: Option<PathBuf>,
) -> Result<()> {
    if file_path.trim().is_empty() {
        bail!("get requires a workspace-relative file path");
    }
    let host_id = resolve_host(client, host).await?;
    let staged = client
        .post(
            &format!("/v1/hosts/{host_id}/files/read"),
            &json!({"workspaceId": workspace, "relPath": file_path}),
        )
        .await?;
    let object_id = staged
        .get("objectId")
        .and_then(Value::as_str)
        .context("host files read returned no objectId")?;
    let size = staged.get("size").and_then(Value::as_u64).unwrap_or(0);
    let digest = staged
        .get("digest")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let bytes = client
        .get_bytes(&format!("/v1/objects/{object_id}"))
        .await
        .with_context(|| format!("downloading {object_id}"))?;
    if let Some(out) = output {
        std::fs::write(&out, &bytes)
            .with_context(|| format!("writing {}", out.display()))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "objectId": object_id,
                "digest": digest,
                "size": size,
                "writtenTo": out.display().to_string(),
            }))?
        );
    } else {
        std::io::stdout().write_all(&bytes)?;
    }
    Ok(())
}
