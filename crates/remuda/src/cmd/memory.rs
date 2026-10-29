//! `remuda memory` — the main agent's per-entry memory (D-060).
//!
//! Entries live on the Hub, keyed by lineage, so they follow the agent across
//! chapters and hosts. A seat runs these with its own credential and no
//! `--lineage`: the lineage is resolved from `REMUDA_INSTANCE_ID`. The owner
//! passes `--lineage`.
//!
//! Every write is a revision compare-and-set: `write` without
//! `--expected-revision` creates (refused when the slug exists), with it
//! updates; `rm` and `consolidate` carry the revisions they replace. Writes
//! print the new revision; soft size limits print the actual numbers.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, hub_http_error, print_json};

/// `remuda memory` subcommands.
#[derive(Args)]
#[command(about = "Read and write the lineage's memory entries on the Hub (revision CAS).")]
pub(crate) struct MemoryArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Lineage id (`ins_…` of its first chapter). Defaults to the lineage of
    /// `REMUDA_INSTANCE_ID`.
    #[arg(long, global = true)]
    lineage: Option<String>,
    /// Print the Hub's JSON response instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: MemoryCommand,
}

impl super::registry::Entrypoint for MemoryArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await }).map(|()| 0)
    }

    fn tracing(&self) -> bool {
        false
    }
}

#[derive(Args)]
struct EntryFlags {
    /// `user`, `feedback`, `project` or `reference`.
    #[arg(long = "type", value_name = "TYPE")]
    memory_type: Option<String>,
    /// One-line title.
    #[arg(long)]
    title: Option<String>,
    /// One-line description (shown in the index).
    #[arg(long)]
    description: Option<String>,
    /// Entry body.
    #[arg(long, group = "body-source")]
    body: Option<String>,
    /// Read the body from a file (`-` = stdin).
    #[arg(long, group = "body-source", value_name = "FILE")]
    body_file: Option<String>,
    /// Journal row the entry derives from, `INSTANCE:SEQ`; repeat for more.
    #[arg(long = "source", value_name = "INSTANCE:SEQ")]
    sources: Vec<String>,
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// Create an entry, or update one with `--expected-revision`.
    Write {
        /// Entry slug: 1-64 of a-z 0-9 . _ - (starts with a letter or digit).
        slug: String,
        /// Revision being replaced; omit to create.
        #[arg(long)]
        expected_revision: Option<u64>,
        #[command(flatten)]
        entry: EntryFlags,
    },
    /// Print one entry, or list the entries when no slug is given.
    Read {
        slug: Option<String>,
        /// Only this type (listing).
        #[arg(long = "type", value_name = "TYPE")]
        memory_type: Option<String>,
        /// Include archived entries (listing).
        #[arg(long)]
        include_archived: bool,
    },
    /// Search titles, descriptions and bodies (full-text, then substring).
    Search {
        /// Query terms; every term must match.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        include_archived: bool,
    },
    /// Delete an entry at the revision you last read.
    Rm {
        slug: String,
        #[arg(long)]
        expected_revision: u64,
    },
    /// Replace two or more entries with one, atomically.
    Consolidate {
        /// Slug of the merged entry (new, or one of the inputs).
        #[arg(long)]
        into: String,
        /// Input entry `SLUG:REVISION`; repeat (at least two).
        #[arg(long = "from", value_name = "SLUG:REVISION", required = true)]
        from: Vec<String>,
        #[command(flatten)]
        entry: EntryFlags,
    },
    /// Print the Hub-rendered index (one line per live entry).
    Index,
    /// Owner only: restore archived entries (all when no slug is given).
    Restore { slugs: Vec<String> },
}

async fn run(args: MemoryArgs) -> anyhow::Result<()> {
    let client = args.hub.connect()?;
    let lineage = resolve_lineage(&client, args.lineage).await?;
    let base = format!("/v1/lineages/{lineage}/memory");
    let json_out = args.json;
    match args.command {
        MemoryCommand::Write {
            slug,
            expected_revision,
            entry,
        } => {
            let value = match expected_revision {
                None => {
                    let body = create_body(&slug, entry)?;
                    client.post(&base, &body).await
                }
                Some(expected) => {
                    let mut body = patch_body(entry)?;
                    body["expectedRevision"] = json!(expected);
                    client.put(&format!("{base}/entries/{slug}"), &body).await
                }
            }
            .map_err(hub_http_error)?;
            report_write(&value, json_out)
        }
        MemoryCommand::Read {
            slug: Some(slug), ..
        } => {
            let value = client
                .get(&format!("{base}/entries/{slug}"))
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            print!("{}", render_entry(&value));
            Ok(())
        }
        MemoryCommand::Read {
            slug: None,
            memory_type,
            include_archived,
        } => {
            let mut query = Vec::new();
            if let Some(kind) = memory_type {
                query.push(format!("type={}", urlencode(&kind)));
            }
            if include_archived {
                query.push("includeArchived=true".into());
            }
            let value = client
                .get(&with_query(&base, &query))
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            for entry in items(&value) {
                println!("{}", summary_line(entry));
            }
            Ok(())
        }
        MemoryCommand::Search {
            query,
            limit,
            include_archived,
        } => {
            let mut params = vec![format!("q={}", urlencode(&query.join(" ")))];
            if let Some(limit) = limit {
                params.push(format!("limit={limit}"));
            }
            if include_archived {
                params.push("includeArchived=true".into());
            }
            let value = client
                .get(&with_query(&format!("{base}/search"), &params))
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            let hits = items(&value);
            if hits.is_empty() {
                eprintln!("no matches");
            }
            for hit in hits {
                println!(
                    "{}  [{}]",
                    summary_line(&hit["entry"]),
                    hit["matchedBy"].as_str().unwrap_or("?")
                );
            }
            Ok(())
        }
        MemoryCommand::Rm {
            slug,
            expected_revision,
        } => {
            let value = client
                .delete(
                    &format!("{base}/entries/{slug}?expectedRevision={expected_revision}"),
                    &Value::Null,
                )
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            println!(
                "deleted {slug} (revision {})",
                value["deleted"]["revision"]
                    .as_u64()
                    .unwrap_or(expected_revision)
            );
            Ok(())
        }
        MemoryCommand::Consolidate { into, from, entry } => {
            let sources = from
                .iter()
                .map(|raw| {
                    let (slug, revision) = split_last(raw, "--from", "SLUG:REVISION")?;
                    Ok(json!({"slug": slug, "expectedRevision": revision}))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let body = json!({"sources": sources, "entry": create_body(&into, entry)?});
            let value = client
                .post(&format!("{base}/consolidate"), &body)
                .await
                .map_err(hub_http_error)?;
            report_write(&value, json_out)
        }
        MemoryCommand::Index => {
            let value = client
                .get(&format!("{base}/index"))
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            print!("{}", value["text"].as_str().unwrap_or(""));
            print_warnings(&value["warnings"]);
            Ok(())
        }
        MemoryCommand::Restore { slugs } => {
            let body = if slugs.is_empty() {
                json!({})
            } else {
                json!({ "slugs": slugs })
            };
            let value = client
                .post(&format!("{base}/restore"), &body)
                .await
                .map_err(hub_http_error)?;
            if json_out {
                return print_json(&value);
            }
            let restored: Vec<&str> = value["restored"]
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            println!("restored {}", restored.len());
            for slug in restored {
                println!("{slug}");
            }
            Ok(())
        }
    }
}

/// `--lineage`, else the lineage of the calling instance.
async fn resolve_lineage(client: &HubClient, flag: Option<String>) -> anyhow::Result<String> {
    if let Some(lineage) = flag.filter(|value| !value.trim().is_empty()) {
        return Ok(lineage);
    }
    let instance = std::env::var("REMUDA_INSTANCE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("pass --lineage <id>: REMUDA_INSTANCE_ID is not set")?;
    let record = client
        .get(&format!("/v1/instances/{instance}"))
        .await
        .map_err(hub_http_error)?;
    record["lineageId"]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("instance {instance} reports no lineageId"))
}

fn read_body(entry: &EntryFlags) -> anyhow::Result<Option<String>> {
    match (&entry.body, &entry.body_file) {
        (Some(text), _) => Ok(Some(text.clone())),
        (None, Some(path)) if path == "-" => Ok(Some(std::io::read_to_string(std::io::stdin())?)),
        (None, Some(path)) => Ok(Some(
            std::fs::read_to_string(path).with_context(|| format!("read body file {path:?}"))?,
        )),
        (None, None) => Ok(None),
    }
}

fn source_refs(entry: &EntryFlags) -> anyhow::Result<Vec<Value>> {
    entry
        .sources
        .iter()
        .map(|raw| {
            let (instance, seq) = split_last(raw, "--source", "INSTANCE:SEQ")?;
            Ok(json!({"instanceId": instance, "seq": seq}))
        })
        .collect()
}

/// `NAME:NUMBER`, split at the last colon.
fn split_last<'a>(raw: &'a str, flag: &str, shape: &str) -> anyhow::Result<(&'a str, u64)> {
    let Some((name, number)) = raw.rsplit_once(':') else {
        bail!("{flag} takes {shape}, got {raw:?}");
    };
    let number = number
        .parse()
        .with_context(|| format!("{flag} takes {shape}, got {raw:?}"))?;
    if name.is_empty() {
        bail!("{flag} takes {shape}, got {raw:?}");
    }
    Ok((name, number))
}

fn create_body(slug: &str, entry: EntryFlags) -> anyhow::Result<Value> {
    let body = read_body(&entry)?.unwrap_or_default();
    let refs = source_refs(&entry)?;
    let (Some(memory_type), Some(title), Some(description)) =
        (entry.memory_type, entry.title, entry.description)
    else {
        bail!(
            "a new entry needs --type, --title and --description \
             (pass --expected-revision N to update an existing one)"
        );
    };
    let mut value = json!({
        "slug": slug,
        "type": memory_type,
        "title": title,
        "description": description,
        "body": body,
    });
    if !refs.is_empty() {
        value["sourceRefs"] = json!(refs);
    }
    Ok(value)
}

fn patch_body(entry: EntryFlags) -> anyhow::Result<Value> {
    let mut value = json!({});
    if let Some(body) = read_body(&entry)? {
        value["body"] = json!(body);
    }
    let refs = source_refs(&entry)?;
    if !refs.is_empty() {
        value["sourceRefs"] = json!(refs);
    }
    if let Some(memory_type) = entry.memory_type {
        value["type"] = json!(memory_type);
    }
    if let Some(title) = entry.title {
        value["title"] = json!(title);
    }
    if let Some(description) = entry.description {
        value["description"] = json!(description);
    }
    Ok(value)
}

fn report_write(value: &Value, json_out: bool) -> anyhow::Result<()> {
    if json_out {
        return print_json(value);
    }
    let entry = &value["entry"];
    println!(
        "{} revision {}",
        entry["slug"].as_str().unwrap_or("?"),
        entry["revision"].as_u64().unwrap_or(0)
    );
    if let Some(removed) = value["removed"].as_array()
        && !removed.is_empty()
    {
        let removed: Vec<&str> = removed.iter().filter_map(Value::as_str).collect();
        println!("removed {}", removed.join(", "));
    }
    print_warnings(&value["warnings"]);
    Ok(())
}

/// Soft-limit warnings with the actual numbers, on stderr.
fn print_warnings(warnings: &Value) {
    for warning in warnings.as_array().into_iter().flatten() {
        let what = match warning["limit"].as_str() {
            Some("entry-soft") => "entry",
            Some("index-soft") => "index",
            _ => "memory",
        };
        eprintln!(
            "warning: {what} is {} bytes (soft limit {} bytes)",
            warning["bytes"].as_u64().unwrap_or(0),
            warning["maxBytes"].as_u64().unwrap_or(0)
        );
    }
}

fn items(value: &Value) -> Vec<&Value> {
    value["items"]
        .as_array()
        .map(|list| list.iter().collect())
        .unwrap_or_default()
}

fn summary_line(entry: &Value) -> String {
    let archived = if entry["archivedAt"].is_string() {
        ", archived"
    } else {
        ""
    };
    format!(
        "- [{}] {}: {} — {} (rev {}{archived})",
        entry["type"].as_str().unwrap_or("?"),
        entry["slug"].as_str().unwrap_or("?"),
        entry["title"].as_str().unwrap_or(""),
        entry["description"].as_str().unwrap_or(""),
        entry["revision"].as_u64().unwrap_or(0),
    )
}

fn render_entry(entry: &Value) -> String {
    let text = |key: &str| entry[key].as_str().unwrap_or("").to_string();
    let mut out = format!(
        "slug: {}\ntype: {}\ntitle: {}\ndescription: {}\nrevision: {}\norigin: {}\nupdated: {}\n",
        text("slug"),
        text("type"),
        text("title"),
        text("description"),
        entry["revision"].as_u64().unwrap_or(0),
        text("origin"),
        text("updatedAt"),
    );
    if let Some(writer) = entry["writerInstance"].as_str() {
        out.push_str(&format!("writer: {writer}\n"));
    }
    if let Some(archived) = entry["archivedAt"].as_str() {
        out.push_str(&format!("archived: {archived}\n"));
    }
    let sources: Vec<String> = entry["sourceRefs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|source| {
            format!(
                "{}:{}",
                source["instanceId"].as_str().unwrap_or("?"),
                source["seq"].as_u64().unwrap_or(0)
            )
        })
        .collect();
    if !sources.is_empty() {
        out.push_str(&format!("sources: {}\n", sources.join(", ")));
    }
    let body = text("body");
    if !body.is_empty() {
        out.push('\n');
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn with_query(path: &str, query: &[String]) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", query.join("&"))
    }
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

    #[test]
    fn source_and_from_split_at_the_last_colon() {
        assert_eq!(
            split_last("ins_a:12", "--source", "INSTANCE:SEQ").unwrap(),
            ("ins_a", 12)
        );
        assert_eq!(
            split_last("tone-1:3", "--from", "SLUG:REVISION").unwrap(),
            ("tone-1", 3)
        );
        for bad in ["ins_a", "ins_a:", ":3", "ins_a:x"] {
            assert!(
                split_last(bad, "--source", "INSTANCE:SEQ").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn queries_are_percent_encoded() {
        assert_eq!(urlencode("边界 a&b"), "%E8%BE%B9%E7%95%8C%20a%26b");
        assert_eq!(with_query("/p", &[]), "/p");
        assert_eq!(
            with_query("/p", &["a=1".into(), "b=2".into()]),
            "/p?a=1&b=2"
        );
    }

    #[test]
    fn entry_rendering_shows_provenance_and_body() {
        let rendered = render_entry(&json!({
            "slug": "tone", "type": "feedback", "title": "Tone", "description": "d",
            "body": "Why: x", "revision": 2, "origin": "agent",
            "updatedAt": "2026-10-29T00:00:00.000Z", "writerInstance": "ins_s",
            "sourceRefs": [{"instanceId": "ins_s", "seq": 4}],
        }));
        assert!(rendered.contains("revision: 2\norigin: agent\n"));
        assert!(rendered.contains("writer: ins_s\n"));
        assert!(rendered.contains("sources: ins_s:4\n"));
        assert!(rendered.ends_with("\nWhy: x\n"));
    }
}
