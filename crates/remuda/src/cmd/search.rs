//! `remuda search <q>` — find journal rows by their words, across the
//! chapters the caller may read (cb-search, protocol.md §8.4).
//!
//! One read-only call to `GET /v1/search`. A seat searches its own lineage's
//! chapters and the workers they dispatched; the owner searches every
//! session. `--instance`, `--lineage` and `--since` narrow it. Each hit names
//! the instance and seq to read next (`remuda instance read <instance>
//! --after-seq <seq-1>`); its snippet is untrusted data. `--status` prints the
//! index backfill's progress instead.

use anyhow::{Context, bail};
use clap::Args;
use serde_json::Value;

use super::hub_client::{HubOpts, block_on, hub_http_error, print_json};
use super::registry::Entrypoint;

/// `remuda search` arguments.
#[derive(Args)]
#[command(about = "Search the journals you may read: your lineage's chapters and their workers.")]
pub(crate) struct SearchArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Query terms (at most 8); every term must match. FTS syntax is literal.
    #[arg(num_args = 0.., required_unless_present = "status")]
    query: Vec<String>,
    /// Only this chapter (`ins_…`).
    #[arg(long)]
    instance: Option<String>,
    /// Only this lineage's chapters (the first chapter's `ins_…`).
    #[arg(long)]
    lineage: Option<String>,
    /// Only rows since this time: RFC 3339, or a duration ago (`30m`, `2h`, `3d`).
    #[arg(long)]
    since: Option<String>,
    /// Hits to return (default 20, at most 50).
    #[arg(long)]
    limit: Option<u32>,
    /// Print the index backfill's progress instead of searching.
    #[arg(long, conflicts_with_all = ["query", "instance", "lineage", "since", "limit"])]
    status: bool,
    /// Print the Hub's JSON response instead of text.
    #[arg(long)]
    json: bool,
}

impl Entrypoint for SearchArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }

    fn tracing(&self) -> bool {
        false
    }
}

async fn run(args: SearchArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    if args.status {
        let value = client
            .get("/v1/search/status")
            .await
            .map_err(hub_http_error)?;
        if args.json {
            print_json(&value)?;
        } else {
            println!("{}", status_line(&value));
        }
        return Ok(0);
    }
    let mut params = vec![format!("q={}", urlencode(&args.query.join(" ")))];
    for (key, value) in [("instance", &args.instance), ("lineage", &args.lineage)] {
        if let Some(value) = value {
            params.push(format!("{key}={}", urlencode(value)));
        }
    }
    if let Some(since) = &args.since {
        let at = since_time(since, time::OffsetDateTime::now_utc())?;
        params.push(format!("since={}", urlencode(&at)));
    }
    if let Some(limit) = args.limit {
        params.push(format!("limit={limit}"));
    }
    let value = client
        .get(&format!("/v1/search?{}", params.join("&")))
        .await
        .map_err(hub_http_error)
        .context("journal search")?;
    if args.json {
        print_json(&value)?;
        return Ok(0);
    }
    let hits = value["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        eprintln!("no matches");
    }
    for hit in &hits {
        println!("{}", hit_line(hit));
    }
    if value["partial"].as_bool() == Some(true) {
        eprintln!(
            "partial: the search ran out of time; narrow it with --instance, --lineage or --since"
        );
    }
    if value["indexComplete"].as_bool() == Some(false) {
        eprintln!("older history is still being indexed (remuda search --status)");
    }
    Ok(0)
}

/// `<instance> seq <seq>  <time>  <kind>  <snippet>`; the lineage is named
/// when it is not the instance itself.
fn hit_line(hit: &Value) -> String {
    let instance = hit["instanceId"].as_str().unwrap_or("?");
    let lineage = hit["lineageId"].as_str().unwrap_or(instance);
    let kind = match (hit["kind"].as_str(), hit["role"].as_str()) {
        (Some(kind), Some(role)) => format!("{kind}/{role}"),
        (Some(kind), None) => kind.to_string(),
        _ => "?".to_string(),
    };
    let chapter = if lineage == instance {
        String::new()
    } else {
        format!(" (lineage {lineage})")
    };
    format!(
        "{instance}{chapter} seq {}  {}  {kind}  {}",
        hit["seq"].as_str().unwrap_or("?"),
        hit["observedAt"].as_str().unwrap_or("?"),
        hit["snippet"].as_str().unwrap_or("")
    )
}

fn status_line(value: &Value) -> String {
    let mut line = format!(
        "index {}",
        if value["complete"].as_bool() == Some(true) {
            "complete"
        } else {
            "backfilling"
        }
    );
    if let Some(state) = value["state"].as_str() {
        line.push_str(&format!(" (state {state})"));
    }
    for (key, label) in [
        ("scannedRows", "scanned"),
        ("journalRows", "of"),
        ("indexedRows", "indexed"),
        ("docs", "docs"),
        ("doneAt", "done at"),
    ] {
        if let Some(text) = value[key].as_str() {
            line.push_str(&format!(", {label} {text}"));
        }
    }
    line
}

/// RFC 3339 passes through; `<n>s|m|h|d` is that long before `now`.
fn since_time(raw: &str, now: time::OffsetDateTime) -> anyhow::Result<String> {
    use time::format_description::well_known::Rfc3339;
    let raw = raw.trim();
    if let Ok(at) = time::OffsetDateTime::parse(raw, &Rfc3339) {
        return Ok(at.format(&Rfc3339)?);
    }
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (count, unit) = raw.split_at(split);
    let Ok(count) = count.parse::<i64>() else {
        bail!("--since takes an RFC 3339 time or a duration such as 30m, 2h or 3d; got {raw:?}");
    };
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            bail!("--since takes an RFC 3339 time or a duration such as 30m, 2h or 3d; got {raw:?}")
        }
    };
    let ago = count
        .checked_mul(seconds)
        .map(time::Duration::seconds)
        .context("--since duration is too long")?;
    let at = now
        .checked_sub(ago)
        .context("--since duration is too long")?;
    Ok(at.format(&Rfc3339)?)
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn since_takes_rfc3339_or_a_duration_ago() {
        let now = time::OffsetDateTime::parse(
            "2026-11-05T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(since_time("2h", now).unwrap(), "2026-11-05T10:00:00Z");
        assert_eq!(since_time("3d", now).unwrap(), "2026-11-02T12:00:00Z");
        assert_eq!(since_time("90s", now).unwrap(), "2026-11-05T11:58:30Z");
        assert_eq!(
            since_time("2026-11-01T08:00:00+08:00", now).unwrap(),
            "2026-11-01T08:00:00+08:00"
        );
        for bad in ["yesterday", "2w", "", "h"] {
            assert!(since_time(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_hit_line_names_where_to_read_next() {
        let hit = json!({
            "instanceId": "ins_b", "lineageId": "ins_a", "seq": "476",
            "observedAt": "2026-11-04T10:19:18.330Z", "kind": "message",
            "role": "assistant", "matchedBy": "fts", "snippet": "rolled back 0042",
        });
        assert_eq!(
            hit_line(&hit),
            "ins_b (lineage ins_a) seq 476  2026-11-04T10:19:18.330Z  message/assistant  rolled back 0042"
        );
        let status = json!({"complete": false, "state": "running", "scannedRows": "4000", "journalRows": "9000"});
        assert_eq!(
            status_line(&status),
            "index backfilling (state running), scanned 4000, of 9000"
        );
    }
}
