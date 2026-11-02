//! cb-memory-guard (main-agent.md §12.5): the secret lint on every memory
//! write, over HTTP.
//!
//! The secrets and the near-misses come from the shared corpus
//! (`scripts/ci/secret-shapes-corpus.txt`), which the gate scanner and the
//! Rust lint already agree on; this suite proves every write path applies it
//! and that a refusal stores nothing and echoes nothing. The corpus spells
//! each needle as `<<NAME>>`, so this file holds no live-looking token.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, RunningHub, spawn};
use serde_json::{Value, json};

const HOST: &str = "hst_guard";
const CORPUS: &str = include_str!("../../../scripts/ci/secret-shapes-corpus.txt");
const SHAPES: &str = include_str!("../../../scripts/ci/secret-shapes.json");

/// (flagged, text) for every corpus line, needles substituted.
fn corpus() -> Vec<(bool, String)> {
    let shapes: Value = serde_json::from_str(SHAPES).expect("shapes");
    let names: Vec<String> = shapes["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .map(|rule| rule["name"].as_str().expect("name").to_string())
        .collect();
    CORPUS
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|raw| {
            let (sign, mut rest) = raw.split_at(2);
            let mut text = String::new();
            while let Some(open) = rest.find("<<") {
                text.push_str(&rest[..open]);
                let close = open + rest[open..].find(">>").expect("closed needle");
                let name = &rest[open + 2..close];
                assert!(names.iter().any(|n| n == name), "unknown rule {name}");
                text.push_str(name);
                rest = &rest[close + 2..];
            }
            text.push_str(rest);
            (sign == "+ ", text)
        })
        .collect()
}

fn secrets() -> Vec<String> {
    corpus()
        .into_iter()
        .filter_map(|(flagged, text)| flagged.then_some(text))
        .collect()
}

fn near_misses() -> Vec<String> {
    corpus()
        .into_iter()
        .filter_map(|(flagged, text)| (!flagged).then_some(text))
        .collect()
}

/// The distinctive tail of a flagged line: what an echo would contain.
fn tail(secret: &str) -> &str {
    let start = secret
        .char_indices()
        .rev()
        .nth(9)
        .map_or(0, |(index, _)| index);
    &secret[start..]
}

struct Ctx {
    _dir: tempfile::TempDir,
    hub: RunningHub,
    http: reqwest::Client,
    human: String,
}

struct Reply {
    status: u16,
    text: String,
    body: Value,
}

impl Ctx {
    async fn spawn() -> Result<Ctx> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        hub.test_insert_host(HOST).await?;
        let human = hub.mint_device_token("guard-owner").await?;
        Ok(Ctx {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
        })
    }

    /// A continuity lineage whose first chapter is current; returns its id.
    async fn seat(&self) -> Result<String> {
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance_delegated(
                HOST.into(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                json!({"origin": "human"}),
                remuda_hub::store_test_support::continuity_delegation(),
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await?;
        Ok(record.instance_id)
    }

    async fn agent(&self, instance: &str) -> Result<String> {
        self.hub
            .test_mint_agent_token(&format!("agent-{instance}"), instance)
            .await
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Reply> {
        let mut request = self
            .http
            .request(method.parse()?, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, text, body })
    }

    async fn ok(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let reply = self.call(method, path, token, body).await?;
        anyhow::ensure!(
            reply.status == 200,
            "{method} {path} -> {}: {}",
            reply.status,
            reply.text
        );
        Ok(reply.body)
    }

    /// Everything a lineage's memory holds, as the owner sees it: entries
    /// (archived included), the index revision and the seat's write log.
    async fn snapshot(&self, lineage: &str, seat: &str) -> Result<Value> {
        let entries = self
            .ok(
                "GET",
                &mem(lineage, "?includeArchived=true"),
                &self.human,
                None,
            )
            .await?;
        let index = self
            .ok("GET", &mem(lineage, "/index"), &self.human, None)
            .await?;
        let writes = self
            .ok(
                "GET",
                &format!("/v1/instances/{seat}/memory-writes"),
                &self.human,
                None,
            )
            .await?;
        Ok(json!({"entries": entries, "index": index, "writes": writes}))
    }
}

fn mem(lineage: &str, rest: &str) -> String {
    format!("/v1/lineages/{lineage}/memory{rest}")
}

fn entry(slug: &str, body: &str) -> Value {
    json!({
        "slug": slug,
        "type": "reference",
        "title": format!("Title {slug}"),
        "description": format!("About {slug}"),
        "body": body,
    })
}

/// A refusal is the documented 422, names the field, and carries no part of
/// the submitted text.
fn assert_refused(reply: &Reply, field: &str, secret: &str) {
    assert_eq!(reply.status, 422, "{}", reply.text);
    assert_eq!(
        reply.body["code"], "MEMORY_SECRET_REJECTED",
        "{}",
        reply.text
    );
    assert_eq!(reply.body["field"], field, "{}", reply.text);
    assert!(
        !reply.text.contains(tail(secret)),
        "the refusal echoes the secret: {}",
        reply.text
    );
}

#[tokio::test]
async fn a_secret_in_any_field_of_a_create_is_refused_unstored_and_unechoed() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let before = ctx.snapshot(&lineage, &lineage).await?;
    // Every secret in every field; the writer alternates between the owner
    // and the seat.
    for (index, secret) in secrets().into_iter().enumerate() {
        let token = if index % 2 == 0 { &ctx.human } else { &seat };
        for field in ["body", "title", "description"] {
            let mut body = entry("gateway", "The gateway answers on port 8443.");
            // One-line fields get the secret inline; the body on a later line.
            body[field] = json!(if field == "body" {
                format!("The gateway answers on port 8443.\n{secret}")
            } else {
                format!("gateway {secret}")
            });
            let reply = ctx
                .call("POST", &mem(&lineage, ""), token, Some(body))
                .await?;
            assert_refused(&reply, field, &secret);
            assert_eq!(reply.body["slug"], "gateway");
        }
    }
    // Nothing was stored: no entry, no revision bump, no write-log row.
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);
    let missing = ctx
        .call("GET", &mem(&lineage, "/entries/gateway"), &ctx.human, None)
        .await?;
    assert_eq!(missing.status, 404, "{}", missing.text);
    Ok(())
}

#[tokio::test]
async fn a_secret_in_the_slug_is_refused_without_naming_the_slug() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    // A slug is lowercase [a-z0-9._-]; the lowercase key shapes fit in one.
    let slug = ["sk", "-proj-tq7wm2xn9bv4lc8rz5hd1kf6"].concat();
    let reply = ctx
        .call(
            "POST",
            &mem(&lineage, ""),
            &ctx.human,
            Some(entry(&slug, "x")),
        )
        .await?;
    assert_refused(&reply, "slug", &slug);
    assert!(reply.body.get("slug").is_none(), "{}", reply.text);
    Ok(())
}

#[tokio::test]
async fn an_update_or_consolidate_target_with_a_secret_changes_nothing() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    for slug in ["gw-1", "gw-2"] {
        ctx.ok(
            "POST",
            &mem(&lineage, ""),
            &seat,
            Some(entry(slug, "port 8443")),
        )
        .await?;
    }
    let before = ctx.snapshot(&lineage, &lineage).await?;
    // Every secret once, rotating the field and the writer.
    for (index, secret) in secrets().into_iter().enumerate() {
        let token = if index % 2 == 0 { &ctx.human } else { &seat };
        let field = ["body", "title", "description"][index % 3];
        let text = if field == "body" {
            format!("port 8443\n\n{secret}")
        } else {
            format!("gw {secret}")
        };
        let reply = ctx
            .call(
                "PUT",
                &mem(&lineage, "/entries/gw-1"),
                token,
                Some(json!({"expectedRevision": 1, field: text})),
            )
            .await?;
        assert_refused(&reply, field, &secret);

        let mut target = entry("gw", "merged");
        target[field] = json!(text);
        let reply = ctx
            .call(
                "POST",
                &mem(&lineage, "/consolidate"),
                token,
                Some(json!({
                    "sources": [{"slug": "gw-1", "expectedRevision": 1},
                                {"slug": "gw-2", "expectedRevision": 1}],
                    "entry": target,
                })),
            )
            .await?;
        assert_refused(&reply, field, &secret);
    }
    // Both inputs are untouched at revision 1 and no target exists.
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);
    Ok(())
}

#[tokio::test]
async fn text_that_merely_resembles_a_secret_is_stored() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let misses = near_misses();
    assert!(misses.len() >= 10);
    for (index, text) in misses.iter().enumerate() {
        let slug = format!("near-{index}");
        let mut body = entry(&slug, &format!("Noted on 2026-10-30.\n{text}"));
        body["description"] = json!(text.replace('\n', " "));
        let written = ctx
            .ok("POST", &mem(&lineage, ""), &seat, Some(body))
            .await?;
        assert_eq!(written["entry"]["revision"], 1);
        let read = ctx
            .ok(
                "GET",
                &mem(&lineage, &format!("/entries/{slug}")),
                &seat,
                None,
            )
            .await?;
        assert!(read["body"].as_str().unwrap().ends_with(text.as_str()));
    }
    // An update and a consolidate of near-misses are stored as well.
    ctx.ok(
        "PUT",
        &mem(&lineage, "/entries/near-0"),
        &ctx.human,
        Some(json!({"expectedRevision": 1, "body": misses[1]})),
    )
    .await?;
    ctx.ok(
        "POST",
        &mem(&lineage, "/consolidate"),
        &seat,
        Some(json!({
            "sources": [{"slug": "near-0", "expectedRevision": 2},
                        {"slug": "near-1", "expectedRevision": 1}],
            "entry": entry("near", &misses[2]),
        })),
    )
    .await?;
    Ok(())
}
