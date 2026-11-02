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

// ─── §12.5 structure: who may read memory ─────────────────────────────────
//
// The memory read interface is the memory module's own: its HTTP handlers and
// the chapter-start renderer live in `src/memory.rs`, and backup is
// `scripts/hub-backup.sh`, which copies the SQLite file whole. Outside that
// module exactly two call sites may touch it, both part of the chapter-start
// renderer (main-agent.md §12.3): the create/resume forward renders the
// chapter's memory context and the change line, and `queue_command` decides
// which input carries the change line. Anything else — an enforcement path
// above all — fails here.
//
// The scan lexes Rust (comments dropped, strings kept, raw strings and char
// literals understood) and works on tokens, so line breaks, spacing, a
// turbofish, a path call, a `use` import or a function pointer all count the
// same as a plain `.memory_get(`. SQL is matched on every string literal of
// every crate after case-folding and collapsing whitespace. Code under
// `#[cfg(test)]` (items, and module files declared with it) is test code.

mod scan {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// The read interface: every `Store` memory read and every read helper of
    /// `src/memory.rs`. Writes are not reads; `archive_lineage` (the retire
    /// transaction) is a write.
    pub const READS: &[&str] = &[
        "memory_get",
        "memory_list",
        "memory_index",
        "memory_search",
        "memory_derived_from",
        "memory_writes",
        "memory_chapter_start",
        "memory_change_rev",
        "claim_change_line",
        "render_index",
        "render_chapter_context",
        "chapter_start_tx",
        "index_rows",
        "load_entry",
        "require_entry",
        "entry_from_row",
        "search_conn",
        "fts_hits",
        "like_hits",
        "write_row",
        "unframe",
    ];

    /// `(file under crates/remuda-hub/src, enclosing fn, read)` allowed
    /// outside the memory module: the chapter-start renderer's call sites.
    pub const ALLOWED: &[(&str, &str, &str)] = &[
        ("http.rs", "forward_if_online", "memory_chapter_start"),
        ("http.rs", "forward_if_online", "memory_change_rev"),
        ("store.rs", "queue_command", "claim_change_line"),
    ];

    const TABLES: &[&str] = &[
        "memory_entries",
        "memory_fts",
        "memory_write_log",
        "memory_slug_revisions",
    ];

    #[derive(Debug, Clone, PartialEq)]
    pub enum Tok {
        Ident(String),
        Str(String),
        Punct(char),
    }

    /// Lex Rust source into tokens with their 1-based line.
    pub fn lex(src: &str) -> Vec<(Tok, usize)> {
        let chars: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        let mut line = 1;
        let ident_start = |c: char| c == '_' || c.is_alphabetic();
        let ident_char = |c: char| c == '_' || c.is_alphanumeric();
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if c == '\n' {
                line += 1;
                i += 1;
            } else if c.is_whitespace() {
                i += 1;
            } else if c == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && next == Some('*') {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if chars[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
            } else if ident_start(c) {
                let start = i;
                while i < chars.len() && ident_char(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let at = chars.get(i).copied();
                // String prefixes: r"..", r#".."#, b"..", br#".."#, c"..".
                let raw = matches!(word.as_str(), "r" | "br" | "cr");
                if raw && matches!(at, Some('"') | Some('#')) {
                    let mut hashes = 0;
                    while chars.get(i) == Some(&'#') {
                        hashes += 1;
                        i += 1;
                    }
                    if chars.get(i) == Some(&'"') {
                        i += 1;
                        let start_line = line;
                        let mut text = String::new();
                        while i < chars.len() {
                            if chars[i] == '"'
                                && (0..hashes).all(|k| chars.get(i + 1 + k) == Some(&'#'))
                            {
                                i += 1 + hashes;
                                break;
                            }
                            if chars[i] == '\n' {
                                line += 1;
                            }
                            text.push(chars[i]);
                            i += 1;
                        }
                        out.push((Tok::Str(text), start_line));
                        continue;
                    }
                    // `r#ident`: a raw identifier.
                    let start = i;
                    while i < chars.len() && ident_char(chars[i]) {
                        i += 1;
                    }
                    out.push((Tok::Ident(chars[start..i].iter().collect()), line));
                    continue;
                }
                if matches!(word.as_str(), "b" | "c") && at == Some('"') {
                    continue; // the string itself is lexed next
                }
                if word == "b" && at == Some('\'') {
                    continue;
                }
                if c.is_ascii_digit() {
                    continue; // numbers carry nothing
                }
                out.push((Tok::Ident(word), line));
            } else if c.is_ascii_digit() {
                while i < chars.len() && (ident_char(chars[i]) || chars[i] == '.') {
                    if chars[i] == '.' && chars.get(i + 1) == Some(&'.') {
                        break;
                    }
                    i += 1;
                }
            } else if c == '"' {
                i += 1;
                let start_line = line;
                let mut text = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                        match chars.get(i) {
                            // A line continuation drops the newline and the
                            // next line's leading whitespace.
                            Some('\n') => {
                                line += 1;
                                i += 1;
                                while i < chars.len() && chars[i].is_whitespace() {
                                    if chars[i] == '\n' {
                                        line += 1;
                                    }
                                    i += 1;
                                }
                                continue;
                            }
                            Some('n') => text.push('\n'),
                            Some('t') => text.push('\t'),
                            Some(other) => text.push(*other),
                            None => {}
                        }
                        i += 1;
                        continue;
                    }
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    text.push(chars[i]);
                    i += 1;
                }
                i += 1;
                out.push((Tok::Str(text), start_line));
            } else if c == '\'' {
                // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
                if next == Some('\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            } else {
                out.push((Tok::Punct(c), line));
                i += 1;
            }
        }
        out
    }

    /// One out-of-place use.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Offender {
        pub file: String,
        pub line: usize,
        pub what: String,
    }

    /// What the scan of one file found: reads of the interface (with their
    /// enclosing fn), SQL reads of the memory tables, and the module files
    /// it declares under `#[cfg(test)]`.
    #[derive(Debug, Default)]
    pub struct FileScan {
        pub reads: Vec<(usize, String, Option<String>)>,
        pub sql: Vec<(usize, String)>,
        pub test_mods: Vec<String>,
    }

    fn is_cfg_test(toks: &[(Tok, usize)], at: usize) -> Option<(usize, bool)> {
        // `#[cfg(test)]` or `#![cfg(test)]`; returns (index after `]`, inner).
        let p = |k: usize, ch: char| matches!(toks.get(k), Some((Tok::Punct(c), _)) if *c == ch);
        let id = |k: usize, s: &str| matches!(toks.get(k), Some((Tok::Ident(w), _)) if w == s);
        if !p(at, '#') {
            return None;
        }
        let (open, inner) = if p(at + 1, '!') {
            (at + 2, true)
        } else {
            (at + 1, false)
        };
        (p(open, '[')
            && id(open + 1, "cfg")
            && p(open + 2, '(')
            && id(open + 3, "test")
            && p(open + 4, ')')
            && p(open + 5, ']'))
        .then_some((open + 6, inner))
    }

    fn is_attribute(toks: &[(Tok, usize)], at: usize) -> bool {
        matches!(toks.get(at), Some((Tok::Punct('#'), _)))
            && matches!(toks.get(at + 1), Some((Tok::Punct('['), _)))
    }

    /// The index after the `#[...]` starting at `at`.
    fn skip_attribute(toks: &[(Tok, usize)], at: usize) -> usize {
        let mut nest = 0usize;
        let mut k = at + 1;
        while k < toks.len() {
            match toks[k].0 {
                Tok::Punct('[') => nest += 1,
                Tok::Punct(']') => {
                    nest -= 1;
                    if nest == 0 {
                        return k + 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        k
    }

    /// Whether the tokens at `at` begin an item (as opposed to a field, a
    /// match arm or a statement).
    fn item_starts(toks: &[(Tok, usize)], mut at: usize) -> bool {
        const ITEMS: &[&str] = &[
            "fn",
            "mod",
            "impl",
            "use",
            "struct",
            "enum",
            "trait",
            "const",
            "static",
            "type",
            "async",
            "unsafe",
            "extern",
            "macro_rules",
        ];
        if matches!(toks.get(at), Some((Tok::Ident(w), _)) if w == "pub") {
            at += 1;
            if matches!(toks.get(at), Some((Tok::Punct('('), _))) {
                while !matches!(toks.get(at), Some((Tok::Punct(')'), _)) | None) {
                    at += 1;
                }
                at += 1;
            }
        }
        matches!(toks.get(at), Some((Tok::Ident(w), _)) if ITEMS.contains(&w.as_str()))
    }

    pub fn scan_tokens(toks: &[(Tok, usize)]) -> FileScan {
        let mut found = FileScan::default();
        let mut depth: usize = 0;
        // (fn name, depth inside its body)
        let mut fns: Vec<(String, usize)> = Vec::new();
        let mut pending_fn: Option<String> = None;
        let mut in_use = false;
        let mut i = 0;
        while i < toks.len() {
            if let Some((after, inner)) = is_cfg_test(toks, i) {
                if inner && depth == 0 {
                    return FileScan::default(); // the whole file is test code
                }
                // Skip what the attribute applies to. An item (fn, mod,
                // impl, use, …) ends at its `;` or with its body; a field,
                // match arm or statement ends at the next `,` or `;`. Either
                // stops before a bracket that closes the enclosing one.
                let mut k = after;
                while is_attribute(toks, k) {
                    k = skip_attribute(toks, k);
                }
                let item = item_starts(toks, k);
                let mut nest = 0usize;
                let mut mod_name = None;
                while k < toks.len() {
                    match &toks[k].0 {
                        Tok::Ident(w) if w == "mod" && nest == 0 => {
                            if let Some((Tok::Ident(name), _)) = toks.get(k + 1) {
                                mod_name = Some(name.clone());
                            }
                        }
                        Tok::Punct('{' | '(' | '[') => nest += 1,
                        Tok::Punct('}' | ')' | ']') if nest == 0 => {
                            k -= 1; // the enclosing bracket is not ours
                            break;
                        }
                        Tok::Punct('}') if nest == 1 && item => {
                            nest = 0;
                            break;
                        }
                        Tok::Punct('}' | ')' | ']') => nest -= 1,
                        Tok::Punct(';') if nest == 0 => {
                            if let Some(name) = mod_name.take() {
                                found.test_mods.push(name);
                            }
                            break;
                        }
                        Tok::Punct(',') if nest == 0 && !item => break,
                        _ => {}
                    }
                    k += 1;
                }
                debug_assert_eq!(nest, 0);
                i = k + 1;
                continue;
            }
            let prev = i.checked_sub(1).map(|k| &toks[k].0);
            match &toks[i].0 {
                Tok::Ident(w) if w == "fn" => {
                    if let Some((Tok::Ident(name), _)) = toks.get(i + 1) {
                        pending_fn = Some(name.clone());
                    }
                }
                Tok::Ident(w) if w == "use" => in_use = true,
                Tok::Punct(';') => {
                    pending_fn = None;
                    in_use = false;
                }
                Tok::Punct('{') => {
                    depth += 1;
                    if let Some(name) = pending_fn.take() {
                        fns.push((name, depth));
                    }
                }
                Tok::Punct('}') => {
                    while fns.last().is_some_and(|(_, d)| *d >= depth) {
                        fns.pop();
                    }
                    depth = depth.saturating_sub(1);
                }
                Tok::Ident(w) if READS.contains(&w.as_str()) => {
                    let path = matches!(prev, Some(Tok::Punct(':')));
                    let method = matches!(prev, Some(Tok::Punct('.')))
                        && matches!(
                            toks.get(i + 1).map(|t| &t.0),
                            Some(Tok::Punct('(')) | Some(Tok::Punct(':'))
                        );
                    let defined = matches!(prev, Some(Tok::Ident(k)) if k == "fn");
                    if (path || method || in_use) && !defined {
                        let enclosing = fns.last().map(|(name, _)| name.clone());
                        found.reads.push((toks[i].1, w.clone(), enclosing));
                    }
                }
                Tok::Str(text) => {
                    if let Some(table) = sql_read(text) {
                        found.sql.push((toks[i].1, table));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        found
    }

    /// A string literal that reads a memory table: `FROM`/`JOIN` it in any
    /// case and spacing, or the bare table name (a query assembled from
    /// pieces).
    pub fn sql_read(text: &str) -> Option<String> {
        let folded = text
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let bare = folded.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        for table in TABLES {
            if bare == *table {
                return Some(table.to_string());
            }
            let words: Vec<&str> = folded
                .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                .filter(|w| !w.is_empty())
                .collect();
            for pair in words.windows(2) {
                let target = pair[1].rsplit('.').next().unwrap_or(pair[1]);
                if matches!(pair[0], "from" | "join") && target == *table {
                    return Some(table.to_string());
                }
            }
        }
        None
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("source dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The file a `mod name;` in `declarer` loads.
    fn module_files(declarer: &Path, name: &str) -> [PathBuf; 2] {
        let stem = declarer.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let dir = if matches!(stem, "lib" | "main" | "mod") {
            declarer.parent().expect("parent").to_path_buf()
        } else {
            declarer.with_extension("")
        };
        [
            dir.join(format!("{name}.rs")),
            dir.join(name).join("mod.rs"),
        ]
    }

    /// Scan `(path, source)` pairs: interface reads only count in the Hub's
    /// own sources (`hub_src`), SQL reads in every crate.
    pub fn offenders(files: &[(PathBuf, String)], hub_src: &Path) -> Vec<Offender> {
        let scans: Vec<FileScan> = files
            .iter()
            .map(|(_, src)| scan_tokens(&lex(src)))
            .collect();
        let mut test_files: BTreeSet<PathBuf> = BTreeSet::new();
        for ((path, _), scan) in files.iter().zip(&scans) {
            for name in &scan.test_mods {
                test_files.extend(module_files(path, name));
            }
        }
        let mut out = Vec::new();
        for ((path, _), scan) in files.iter().zip(&scans) {
            if test_files.contains(path) {
                continue;
            }
            let in_hub = path.strip_prefix(hub_src).ok();
            let relative = in_hub
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            if in_hub.is_some_and(|p| p == Path::new("memory.rs")) {
                continue;
            }
            if in_hub.is_some() {
                for (line, read, enclosing) in &scan.reads {
                    let allowed = ALLOWED.iter().any(|(file, func, name)| {
                        *file == relative && Some(*func) == enclosing.as_deref() && name == read
                    });
                    if !allowed {
                        out.push(Offender {
                            file: relative.clone(),
                            line: *line,
                            what: format!(
                                "{read} in fn {}",
                                enclosing.as_deref().unwrap_or("<none>")
                            ),
                        });
                    }
                }
            }
            for (line, table) in &scan.sql {
                out.push(Offender {
                    file: relative.clone(),
                    line: *line,
                    what: format!("SQL read of {table}"),
                });
            }
        }
        out.sort();
        out
    }

    /// Every production Rust source of the workspace.
    pub fn workspace() -> (Vec<(PathBuf, String)>, PathBuf) {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let crates = manifest.parent().expect("crates dir");
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(crates).expect("crates") {
            let src = entry.expect("crate").path().join("src");
            if src.is_dir() {
                rust_files(&src, &mut paths);
            }
        }
        paths.sort();
        let files = paths
            .into_iter()
            .map(|path| {
                let source = std::fs::read_to_string(&path).expect("source");
                (path, source)
            })
            .collect();
        (files, manifest.join("src"))
    }
}

#[test]
fn the_memory_read_interface_has_no_caller_outside_the_allowed_sites() {
    let (files, hub_src) = scan::workspace();
    assert!(files.len() > 100, "scanned {} files", files.len());
    let offenders = scan::offenders(&files, &hub_src);
    assert!(
        offenders.is_empty(),
        "memory is read outside the memory module and the chapter-start renderer \
         (main-agent.md §12.5): {offenders:#?}"
    );
    // The allowlist is live: every allowed site is still where it says, so
    // the scan is seeing the real calls and a stale entry cannot hide one.
    for (file, func, read) in scan::ALLOWED {
        let (_, source) = files
            .iter()
            .find(|(path, _)| path.strip_prefix(&hub_src).ok() == Some(std::path::Path::new(file)))
            .expect("allowed file");
        let found = scan::scan_tokens(&scan::lex(source));
        assert!(
            found
                .reads
                .iter()
                .any(|(_, name, enclosing)| name == read && enclosing.as_deref() == Some(*func)),
            "{file}: {func} no longer calls {read}; update ALLOWED"
        );
    }
}

/// The scan is robust to formatting and does not cry wolf. Every source here
/// is placed at `crates/remuda-hub/src/agent_scope.rs` (an enforcement module)
/// unless it says otherwise.
#[test]
fn the_structure_scan_catches_out_of_place_reads_however_written() {
    let hub_src = std::path::PathBuf::from("/w/crates/remuda-hub/src");
    let check = |file: &str, source: &str| -> Vec<String> {
        let path = if file.starts_with('/') {
            std::path::PathBuf::from(file)
        } else {
            hub_src.join(file)
        };
        scan::offenders(&[(path, source.to_string())], &hub_src)
            .into_iter()
            .map(|o| o.what)
            .collect()
    };
    let caught = [
        // A plain call, then the same split over lines with odd spacing.
        "async fn require_grant(s: &AppState) { s.store.memory_get(a, b).await; }",
        "async fn require_grant(s: &AppState) {\n    s\n        .store\n        .memory_get\n        (\n a, b)\n        .await;\n}",
        // A turbofish, a path call, a function pointer, a use import.
        "fn f() { s . memory_list :: < X > (a); }",
        "fn f() { crate::memory::render_index(conn, id, false); }",
        "fn f() { let read = Store::memory_search; }",
        "use crate::memory::{self, load_entry};\nfn f() {}",
        "use crate::memory::{\n    ChapterMemory,\n    claim_change_line,\n};",
        // An allowed read, but from the wrong function of the allowed file.
        "",
        // SQL in any case and spacing, raw strings, pieces, a schema prefix.
        "fn f() { conn.query_row(\"SELECT body\\n   FROM\\n\\tmemory_entries WHERE slug = ?1\", p, r); }",
        "fn f() { conn.prepare(r#\"select * from \"memory_entries\" m\"#); }",
        "fn f() { conn.prepare(concat!(\"SELECT body FROM \", \"memory_entries\")); }",
        "fn f() { conn.prepare(\"SELECT rowid FROM t JOIN main.memory_fts f ON 1\"); }",
        "fn f() { conn.prepare(\"SELECT * FROM \\\n      memory_write_log\"); }",
        // Test-only code next to production code does not hide the latter.
        "fn f() { match x {\n    #[cfg(test)]\n    A => 1,\n    B => s.memory_get(a, b),\n} }",
        "struct S {\n    #[cfg(test)]\n    a: u8,\n}\nfn f() { s.memory_get(a, b); }",
        "#[cfg(test)]\n#[allow(unused)]\nuse x::y;\nfn f() { s.memory_get(a, b); }",
    ];
    for source in caught {
        if source.is_empty() {
            let misplaced = "pub(crate) async fn create_instance(state: &AppState) {\n\
                             state.store.memory_chapter_start(id, host).await;\n}";
            assert_eq!(
                check("http.rs", misplaced),
                ["memory_chapter_start in fn create_instance"]
            );
            continue;
        }
        assert!(
            !check("agent_scope.rs", source).is_empty(),
            "missed: {source}"
        );
    }
    // Another crate reading the tables directly is caught too.
    assert_eq!(
        check(
            "/w/crates/remuda/src/cmd/peek.rs",
            "fn f() { db.query(\"SELECT title FROM memory_entries\"); }"
        ),
        ["SQL read of memory_entries"]
    );

    let clean = [
        // Comments, a field of the same name, a definition, docs, test code.
        "fn f() { // s.store.memory_get(a, b)\n /* .memory_list( */ }",
        "fn f(row: &Row) { let v = chapter.memory_index; let w = Row { memory_index: x }; }",
        "pub memory_index: Option<String>,",
        "fn f() { conn.execute(\"UPDATE lineages SET memory_rev = memory_rev + 1\", p); }",
        "fn f() { conn.execute(\"DROP TABLE memory_entries\", p); }",
        "#[cfg(test)]\nmod tests { fn t() { s.store.memory_get(a, b); } }",
        "#[cfg(test)]\nfn t() -> Result<(), E> { s.store.memory_list(a, None, true) }",
        "fn f() { let s = '\"'; let l: &'static str = \"from memory\"; }",
        "struct S {\n    #[cfg(test)]\n    park: Option<HashMap<String, u64>>,\n    other: u8,\n}",
    ];
    for source in clean {
        assert!(
            check("agent_scope.rs", source).is_empty(),
            "false alarm: {source}"
        );
    }
    // The allowed sites themselves, and the memory module.
    assert!(
        check(
            "http.rs",
            "pub(crate) async fn forward_if_online(state: &AppState) {\n\
             if let Some(rev) = state.store.memory_change_rev(id).await? {}\n\
             match state.store.memory_chapter_start(id, host).await? {}\n}"
        )
        .is_empty()
    );
    assert!(check("memory.rs", "fn f() { load_entry(conn, l, s); }").is_empty());
    // A module file declared under cfg(test) is test code; the same file
    // declared without it is not.
    let lib = hub_src.join("lib.rs");
    let module = hub_src.join("probe.rs");
    let read = "fn f() { s.store.memory_get(a, b); }".to_string();
    let files = |decl: &str| {
        vec![
            (lib.clone(), decl.to_string()),
            (module.clone(), read.clone()),
        ]
    };
    assert!(scan::offenders(&files("#[cfg(test)]\nmod probe;"), &hub_src).is_empty());
    assert_eq!(scan::offenders(&files("mod probe;"), &hub_src).len(), 1);
}

// ─── The Phase 1 import route ─────────────────────────────────────────────

fn import_entry(slug: &str, origin: &str, body: &str) -> Value {
    json!({"slug": slug, "type": "project", "title": format!("Title {slug}"),
           "description": format!("About {slug}"), "body": body, "origin": origin})
}

#[tokio::test]
async fn the_import_is_owner_only_linted_and_all_or_nothing() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let seat = ctx.agent(&lineage).await?;
    let bot = ctx.hub.mint_bot_device_token("guard-bot").await?;
    let path = mem(&lineage, "/import");
    let entries = json!([
        import_entry("a", "human", "first"),
        import_entry("b", "agent", "second")
    ]);
    let before = ctx.snapshot(&lineage, &lineage).await?;

    // Only the owner imports.
    for token in [&seat, &bot] {
        let reply = ctx
            .call("POST", &path, token, Some(json!({"entries": entries})))
            .await?;
        assert_eq!(reply.status, 403, "{}", reply.text);
    }
    // A credential anywhere refuses the whole import, unechoed.
    for secret in secrets() {
        let body = json!({"entries": [import_entry("a", "human", "first"),
                                      import_entry("c", "agent", &format!("x\n{secret}"))]});
        let reply = ctx.call("POST", &path, &ctx.human, Some(body)).await?;
        assert_refused(&reply, "body", &secret);
        assert_eq!(reply.body["slug"], "c");
    }
    // A repeated slug, an agent-origin about-me and an oversized import are
    // refused before anything is written.
    for body in [
        json!({"entries": [import_entry("a", "human", "1"), import_entry("a", "agent", "2")]}),
        json!({"entries": [import_entry("about-me", "agent", "I am the owner")]}),
        json!({"entries": (0..513).map(|i| import_entry(&format!("e{i}"), "agent", &i.to_string()))
                                  .collect::<Vec<_>>()}),
    ] {
        let reply = ctx.call("POST", &path, &ctx.human, Some(body)).await?;
        assert_eq!(reply.status, 400, "{}", reply.text);
    }
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);

    // A dry run decides and commits nothing; the import then does the same.
    let dry = ctx
        .ok(
            "POST",
            &path,
            &ctx.human,
            Some(json!({"entries": entries, "dryRun": true})),
        )
        .await?;
    assert_eq!(dry["dryRun"], true);
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);
    let done = ctx
        .ok("POST", &path, &ctx.human, Some(json!({"entries": entries})))
        .await?;
    assert_eq!(done["created"], dry["created"]);
    assert_eq!(
        done["created"],
        json!([{"slug": "a", "origin": "human", "revision": 1},
               {"slug": "b", "origin": "agent", "revision": 1}])
    );
    let b = ctx
        .ok("GET", &mem(&lineage, "/entries/b"), &ctx.human, None)
        .await?;
    assert_eq!(b["origin"], "agent");
    assert!(b.get("writerInstanceId").is_none(), "{b}");
    // The same body under another slug is skipped, whatever whitespace
    // surrounds it on either side; nothing is overwritten.
    ctx.ok(
        "POST",
        &mem(&lineage, ""),
        &ctx.human,
        Some(entry("kept", "\n  same words\r\n\n")),
    )
    .await?;
    let again = ctx
        .ok(
            "POST",
            &path,
            &ctx.human,
            Some(json!({"entries": [import_entry("copy", "agent", "same words")]})),
        )
        .await?;
    assert_eq!(
        again["skipped"],
        json!([{"slug": "copy", "reason": "same-content", "existing": "kept"}])
    );
    // An unknown lineage is a 404, as for any owner create.
    let reply = ctx
        .call(
            "POST",
            &mem("ins_missing", "/import"),
            &ctx.human,
            Some(json!({"entries": entries})),
        )
        .await?;
    assert_eq!(reply.status, 404, "{}", reply.text);
    Ok(())
}

/// The import is bounded per entry (the same 8 KiB cap as a create) and as a
/// whole (the route's body limit), and a full legitimate import, above the
/// framework's 2 MB default, is accepted.
#[tokio::test]
async fn the_import_is_bounded_per_entry_and_per_request() -> Result<()> {
    let ctx = Ctx::spawn().await?;
    let lineage = ctx.seat().await?;
    let path = mem(&lineage, "/import");
    let before = ctx.snapshot(&lineage, &lineage).await?;

    // One entry over the cap refuses the import: 413, nothing stored.
    let big = "y".repeat(8 * 1024 + 1);
    let reply = ctx
        .call(
            "POST",
            &path,
            &ctx.human,
            Some(json!({"entries": [import_entry("small", "agent", "fine"),
                                    import_entry("big", "agent", &big)]})),
        )
        .await?;
    assert_eq!(reply.status, 413, "{}", reply.text);
    assert_eq!(
        reply.body["code"], "MEMORY_ENTRY_TOO_LARGE",
        "{}",
        reply.text
    );
    assert_eq!(reply.body["slug"], "big");
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);

    // A body over the route limit is refused before it is parsed.
    let limit = 512 * (2 * 8 * 1024 + 1024) + 4096;
    let padding = "z".repeat(limit + 1);
    let response = ctx
        .http
        .post(format!("http://{}{path}", ctx.hub.addr))
        .bearer_auth(&ctx.human)
        .header("content-type", "application/json")
        .body(format!(r#"{{"entries":[],"pad":"{padding}"}}"#))
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 413);
    assert_eq!(ctx.snapshot(&lineage, &lineage).await?, before);

    // 512 entries just under the cap (over 3 MiB of JSON) are accepted.
    let entries: Vec<Value> = (0..512)
        .map(|i| {
            let body = format!("{i:04} {}", "w".repeat(8 * 1024 - 200));
            import_entry(&format!("full-{i}"), "agent", &body)
        })
        .collect();
    let request = json!({"entries": entries, "dryRun": true});
    assert!(request.to_string().len() > 3 * 1024 * 1024);
    let full = ctx.ok("POST", &path, &ctx.human, Some(request)).await?;
    assert_eq!(full["created"].as_array().map(Vec::len), Some(512));
    Ok(())
}
