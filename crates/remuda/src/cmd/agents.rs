//! Live instance table: follow events plus periodic registry reconciliation.

use super::hub_client::{HubClient, HubOpts, block_on, print_json};
use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    sync::Arc,
    time::Duration,
};

#[derive(Debug, Clone, Default, Args)]
pub(crate) struct ListArgs {
    /// Restrict the table to one host id.
    #[arg(long)]
    pub host: Option<String>,
    /// Keep the table updated from the Hub follow WebSocket; Ctrl-C stops.
    #[arg(long)]
    pub watch: bool,
    /// Emit JSON (newline-delimited snapshots when watching).
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
struct Tail {
    seq: u64,
    line: String,
}

fn sequence(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

pub(crate) fn run(hub: HubOpts, args: ListArgs) -> Result<()> {
    block_on(async move { list(Arc::new(hub.connect()?), args).await })
}

pub(crate) async fn list(client: Arc<HubClient>, args: ListArgs) -> Result<()> {
    let mut lines = BTreeMap::<String, Tail>::new();
    if !args.watch {
        let rows = refresh(&client, args.host.as_deref(), &mut lines).await?;
        return display(&rows, &args, "snapshot");
    }
    let mut shutdown = crate::Shutdown::install()?;
    let mut rows = Vec::new();
    loop {
        let connection = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(10), client.follow_all_ws()) => result,
            result = shutdown.wait() => { result?; return Ok(()); }
        };
        let mut socket = match connection {
            Ok(Ok(socket)) => socket,
            _ => {
                display(&rows, &args, "disconnected")?;
                tokio::select! { _ = tokio::time::sleep(Duration::from_secs(1)) => {}, result = shutdown.wait() => { result?; return Ok(()); } }
                continue;
            }
        };
        // Every reconnect re-reads tails and authoritative lifecycle/link state.
        lines.clear();
        let mut dirty = true;
        let mut force_tails = false;
        let mut refresh_tick = tokio::time::interval(Duration::from_millis(250));
        let mut registry_tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                result = shutdown.wait() => { result?; let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await; return Ok(()); }
                frame = socket.next_json() => {
                    match frame {
                        Ok(Some(frame)) => {
                            if frame["type"] == "gap" { force_tails = true; }
                            apply_frame(&frame, &mut lines);
                            dirty = true;
                        }
                        _ => { display(&rows, &args, "disconnected")?; break; }
                    }
                }
                _ = registry_tick.tick() => { dirty = true; }
                _ = refresh_tick.tick(), if dirty => {
                    if force_tails { lines.clear(); force_tails = false; }
                    let next = tokio::select! {
                        result = tokio::time::timeout(Duration::from_secs(10), refresh(&client, args.host.as_deref(), &mut lines)) => result,
                        result = shutdown.wait() => { result?; let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await; return Ok(()); }
                    };
                    match next {
                        Ok(Ok(next)) => { rows = next; display(&rows, &args, "connected")?; }
                        _ => { display(&rows, &args, "stale")?; let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await; break; }
                    }
                    dirty = false;
                }
            }
        }
        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(1)) => {}, result = shutdown.wait() => { result?; return Ok(()); } }
    }
}

async fn refresh(
    client: &Arc<HubClient>,
    host: Option<&str>,
    lines: &mut BTreeMap<String, Tail>,
) -> Result<Vec<Value>> {
    let (hosts, mut items) = tokio::try_join!(client.list_hosts(), client.list_instances())?;
    items.retain(|item| host.is_none_or(|id| item["hostId"] == id));
    let mut tails = tokio::task::JoinSet::new();
    for item in &items {
        let Some(id) = item["instanceId"].as_str() else {
            continue;
        };
        if !lines.contains_key(id) {
            if tails.len() >= 8 {
                save_tail(tails.join_next().await, lines)?;
            }
            let id = id.to_owned();
            let after = sequence(&item["durableSeq"]).saturating_sub(32).to_string();
            let client = Arc::clone(client);
            tails.spawn(async move {
                let page = client.get_journal(&id, Some(&after)).await;
                let tail = page.ok().map(|page| Tail {
                    seq: sequence(&page["durableSeq"]),
                    line: page["events"]
                        .as_array()
                        .and_then(|events| events.iter().rev().find_map(last_line))
                        .unwrap_or_default(),
                });
                (id, tail)
            });
        }
    }
    while !tails.is_empty() {
        save_tail(tails.join_next().await, lines)?;
    }
    lines.retain(|id, _| items.iter().any(|item| item["instanceId"] == *id));
    let mut rows: Vec<_> = items
        .iter()
        .map(|item| {
            let mut row = super::instance::project_instance(item, &hosts);
            let id = item["instanceId"].as_str().unwrap_or("");
            row["lastLine"] = json!(lines.get(id).map(|tail| tail.line.as_str()).unwrap_or(""));
            row["lastLineAvailable"] = json!(lines.contains_key(id));
            row
        })
        .collect();
    rows.sort_by(|a, b| {
        (
            a["host"].as_str(),
            a["name"].as_str(),
            a["instanceId"].as_str(),
        )
            .cmp(&(
                b["host"].as_str(),
                b["name"].as_str(),
                b["instanceId"].as_str(),
            ))
    });
    Ok(rows)
}

fn save_tail(
    result: Option<std::result::Result<(String, Option<Tail>), tokio::task::JoinError>>,
    lines: &mut BTreeMap<String, Tail>,
) -> Result<()> {
    if let Some(result) = result {
        let (id, tail) = result.context("journal tail task")?;
        if let Some(tail) = tail {
            store_tail(lines, &id, tail);
        }
    }
    Ok(())
}

fn apply_frame(frame: &Value, lines: &mut BTreeMap<String, Tail>) {
    let Some(id) = frame["instanceId"].as_str() else {
        return;
    };
    let line = if frame["type"] == "snapshot" {
        frame["events"]
            .as_array()
            .and_then(|events| events.iter().rev().find_map(last_line))
    } else {
        last_line(&frame["event"])
    };
    if let Some(line) = line {
        let seq = if frame["type"] == "snapshot" {
            &frame["asOfSeq"]
        } else {
            &frame["seq"]
        };
        store_tail(
            lines,
            id,
            Tail {
                seq: sequence(seq),
                line,
            },
        );
    }
}

fn store_tail(lines: &mut BTreeMap<String, Tail>, id: &str, tail: Tail) {
    // REST tails can be newer than the follow frames buffered during refresh.
    if lines.get(id).is_none_or(|current| tail.seq >= current.seq) {
        lines.insert(id.into(), tail);
    }
}

fn last_line(value: &Value) -> Option<String> {
    fn text(value: &Value) -> Option<&str> {
        match value {
            Value::Object(map) => {
                if map.get("role").and_then(Value::as_str) == Some("user")
                    || map.get("direction").and_then(Value::as_str) == Some("input")
                {
                    return None;
                }
                for key in ["text", "output", "screen"] {
                    if let Some(text) = map
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|v| !v.trim().is_empty())
                    {
                        return Some(text);
                    }
                }
                if map.get("nativeName").and_then(Value::as_str) == Some("screen") {
                    let status = map.get("status")?;
                    return status.as_str().or_else(|| {
                        (status["state"] == "known")
                            .then(|| status["value"].as_str())
                            .flatten()
                    });
                }
                map.iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "input" | "initialInput" | "request" | "prompt"
                        )
                    })
                    .find_map(|(_, value)| text(value))
            }
            Value::Array(items) => items.iter().rev().find_map(text),
            _ => None,
        }
    }
    let line = text(value)?
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?;
    Some(super::table::cell(line, 200))
}

fn display(rows: &[Value], args: &ListArgs, stream: &str) -> Result<()> {
    if args.json {
        let snapshot =
            json!({"items":rows,"stream":stream,"stale":matches!(stream,"stale"|"disconnected")});
        if args.watch {
            let mut output = std::io::stdout().lock();
            writeln!(output, "{}", serde_json::to_string(&snapshot)?)?;
            output.flush()?;
            return Ok(());
        }
        return print_json(&snapshot);
    }
    let mut output = std::io::stdout().lock();
    if args.watch && std::io::stdout().is_terminal() {
        write!(output, "\x1b[2J\x1b[H")?;
    }
    writeln!(
        output,
        "Instances ({stream}){}",
        if matches!(stream, "stale" | "disconnected") {
            " — displayed state is stale"
        } else {
            ""
        }
    )?;
    let data: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            [
                "name",
                "host",
                "kind",
                "lifecycle",
                "activity",
                "connectivity",
                "worktree",
                "lastLine",
            ]
            .iter()
            .map(|key| row[*key].as_str().unwrap_or("-").to_owned())
            .collect()
        })
        .collect();
    write!(
        output,
        "{}",
        super::table::render(
            &[
                "NAME",
                "HOST",
                "KIND",
                "LIFECYCLE",
                "ACTIVITY",
                "LINK",
                "WORKTREE",
                "LAST LINE"
            ],
            &[18, 18, 8, 12, 12, 13, 28, 60],
            &data
        )
    )?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buffered_follow_cannot_overwrite_a_newer_rest_tail() {
        let mut lines = BTreeMap::from([(
            "ins_a".into(),
            Tail {
                seq: 20,
                line: "current".into(),
            },
        )]);
        apply_frame(
            &json!({"type":"event","instanceId":"ins_a","seq":"10","event":{"text":"old"}}),
            &mut lines,
        );
        assert_eq!(lines["ins_a"].line, "current");
        apply_frame(
            &json!({"type":"event","instanceId":"ins_a","seq":"21","event":{"text":"new"}}),
            &mut lines,
        );
        assert_eq!(lines["ins_a"].line, "new");
    }
    #[test]
    fn follow_text_is_bounded_and_does_not_display_metadata_or_terminal_controls() {
        let mut lines = BTreeMap::new();
        apply_frame(
            &json!({"type":"event","instanceId":"ins_a","event":{"event":{"text":"first\n\u{1b}[31mDONE\u{1b}[0m"}}}),
            &mut lines,
        );
        assert_eq!(lines["ins_a"].line, "DONE");
        assert_eq!(
            last_line(&json!({"type":"lifecycle","hostId":"hst_a"})),
            None
        );
        assert_eq!(
            last_line(&json!({"request":{"input":{"text":"do task"}}})),
            None
        );
        assert_eq!(last_line(&json!({"payload":{"nativeName":"screen","status":{"state":"known","value":"first\nlast"}}})).as_deref(), Some("last"));
        assert_eq!(
            last_line(&json!({"role":"user","content":{"text":"prompt"}})),
            None
        );
    }
}
