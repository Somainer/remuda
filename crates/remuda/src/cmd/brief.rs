//! `remuda brief` — lint a worker brief and re-deliver a brief file to a live
//! worker (M1 batch 5a; coordinator-hierarchy.md §2.4, coordinator-guide.md
//! "Secret scan").
//!
//! Briefs are the coordinator's only reliable channel to a fresh worker, so
//! they are linted before dispatch:
//! - no backticks (a brief travels as a file, never through a shell; a
//!   backtick in an inline prompt would be executed by the remote shell);
//! - no personal absolute home paths (`/home/<user>`, `/Users/<user>`);
//! - no private hostname/username matching the secret-scan denylist;
//! - the `DONE <sha>` / `BLOCKED <reason>` reply contract is present.

use clap::{Args, Subcommand};
use regex::Regex;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::OnceLock;

use super::hub_client::{HubOpts, block_on, print_json};

/// Hashed denylist shared with `scripts/ci/secret-scan.py` (plaintext never
/// enters the repo). Embedded at compile time so lint works off-tree.
const PRIVATE_TOKENS_SHA256: &str = include_str!("../../../../scripts/ci/private-tokens.sha256");

/// One lint violation (1-based line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// 1-based source line.
    pub line: usize,
    /// Stable rule id (`backtick` / `home-path` / `private-token` /
    /// `missing-contract`).
    pub rule: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

fn violation(line: usize, rule: &'static str, message: impl Into<String>) -> Violation {
    Violation {
        line,
        rule,
        message: message.into(),
    }
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn denylist() -> &'static HashSet<&'static str> {
    static DENYLIST: OnceLock<HashSet<&'static str>> = OnceLock::new();
    DENYLIST.get_or_init(|| {
        PRIVATE_TOKENS_SHA256
            .lines()
            .map(str::trim)
            .filter(|line| line.len() == 64 && !line.starts_with('#'))
            .collect()
    })
}

// Candidate shapes, ported from scripts/ci/secret-scan.py so the CLI lint and
// the gate scan reject the same text.
fn host_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,24}\b")
            .expect("host regex")
    })
}

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,24}\b").expect("email regex")
    })
}

fn home_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:/home|/Users)/([A-Za-z0-9._-]+)").expect("home regex"))
}

fn ident_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b[a-z][a-z0-9_-]{4,39}\b").expect("ident regex"))
}

/// Source-file suffix labels the scanner treats as filenames, not hosts.
const SOURCE_SUFFIX_LABELS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "md", "toml", "json", "yml", "yaml", "css", "lock", "c",
    "h", "go", "rb",
];

/// Mirror of secret-scan.py `host_expansions`: every label/suffix that might be
/// the private token as stored.
fn host_expansions(host: &str) -> HashSet<String> {
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host
        .trim_matches('.')
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    let mut out = HashSet::new();
    out.insert(host.clone());
    if labels
        .last()
        .is_some_and(|label| SOURCE_SUFFIX_LABELS.contains(label))
    {
        return out;
    }
    for label in &labels {
        if label.len() >= 3 && !SOURCE_SUFFIX_LABELS.contains(label) {
            out.insert((*label).to_string());
        }
    }
    for i in 0..labels.len().saturating_sub(1) {
        let suffix = labels[i..].join(".");
        if !suffix.is_empty() {
            out.insert(suffix);
        }
    }
    out
}

fn expansions_for(raw: &str) -> HashSet<String> {
    let value = raw.trim().trim_matches('\'').trim_matches('"');
    let lower = value.to_ascii_lowercase();
    let mut out = HashSet::new();
    out.insert(lower.clone());
    if let Some((local, domain)) = lower.split_once('@') {
        if !local.is_empty() {
            out.insert(local.to_string());
        }
        if !domain.is_empty() {
            out.extend(host_expansions(domain));
        }
    } else {
        out.extend(host_expansions(&lower));
    }
    out.retain(|item| item.len() >= 3);
    out
}

/// The completion contract a brief must instruct: both possible final lines.
fn contract_present(text: &str) -> bool {
    let has_done = Regex::new(r"(?i)DONE\s+<sha>")
        .expect("regex")
        .is_match(text);
    let has_blocked = Regex::new(r"(?i)BLOCKED\s+<reason>")
        .expect("regex")
        .is_match(text);
    has_done && has_blocked
}

/// Lint brief text; returns every violation (empty = dispatchable).
pub fn lint_brief(text: &str) -> Vec<Violation> {
    let mut violations = Vec::new();
    let denylist = denylist();

    for (index, line) in text.split('\n').enumerate() {
        let line_no = index + 1;
        if line.contains('`') {
            violations.push(violation(
                line_no,
                "backtick",
                "briefs are delivered as files, never through a shell; a backtick could be \
                 executed on the remote host — quote with plain text instead",
            ));
        }
        if let Some(capture) = home_re().captures(line) {
            let user = capture.get(1).expect("group").as_str();
            // Any literal home path is a leak: the worker gets $HOME-relative
            // or product-assigned paths, never the operator's home.
            violations.push(violation(
                line_no,
                "home-path",
                format!(
                    "personal home path /…/{user}: normalize to $HOME or a $WORKTREE-relative path"
                ),
            ));
        }
        let mut candidates: Vec<String> = Vec::new();
        candidates.extend(email_re().find_iter(line).map(|m| m.as_str().to_string()));
        candidates.extend(host_re().find_iter(line).map(|m| m.as_str().to_string()));
        candidates.extend(
            home_re()
                .captures_iter(line)
                .filter_map(|capture| capture.get(1).map(|m| m.as_str().to_string())),
        );
        candidates.extend(ident_re().find_iter(line).map(|m| m.as_str().to_string()));
        for candidate in candidates {
            let mut hit = None;
            for piece in expansions_for(&candidate) {
                if denylist.contains(sha256_hex(&piece).as_str()) {
                    hit = Some(piece.len());
                    break;
                }
            }
            if hit.is_some() {
                violations.push(violation(
                    line_no,
                    "private-token",
                    format!(
                        "private hostname/username token matched the secret-scan denylist near {:?}; \
                         use a placeholder",
                        candidate.chars().take(24).collect::<String>()
                    ),
                ));
            }
        }
    }

    if text.trim().is_empty() {
        violations.push(violation(0, "empty", "brief is empty"));
    }
    if !contract_present(text) {
        violations.push(violation(
            0,
            "missing-contract",
            "brief must instruct the worker to reply on one line with `DONE <sha>` or \
             `BLOCKED <reason>`",
        ));
    }
    violations
}

// ── CLI ────────────────────────────────────────────────────────────────────

/// `remuda brief` subcommands.
#[derive(Args)]
#[command(about = "Lint and deliver worker briefs.")]
pub(crate) struct BriefArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: BriefCommand,
}

#[derive(Subcommand)]
enum BriefCommand {
    /// Validate a brief file (backticks, home paths, private tokens, contract).
    Lint {
        /// Brief markdown file (`-` = stdin).
        file: String,
        /// Emit JSON even when clean.
        #[arg(long)]
        json: bool,
    },
    /// Re-deliver a brief/handback file to a live worker (file transport).
    Send {
        /// Worker name or `wkr_…` roster id.
        worker: String,
        /// Brief file to deliver.
        file: String,
        /// Override the attachment name.
        #[arg(long)]
        name: Option<String>,
    },
}

impl super::registry::Entrypoint for BriefArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self.hub, self.command).await })
    }
}

async fn run(hub: HubOpts, command: BriefCommand) -> anyhow::Result<i32> {
    match command {
        BriefCommand::Lint { file, json } => {
            let text = if file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&file)
                    .map_err(|err| anyhow::anyhow!("read brief {file:?}: {err}"))?
            };
            let violations = lint_brief(&text);
            if json {
                let report = json!({
                    "ok": violations.is_empty(),
                    "violations": violations.iter().map(|v| json!({
                        "line": v.line, "rule": v.rule, "message": v.message,
                    })).collect::<Vec<_>>(),
                });
                print_json(&report)?;
            }
            if violations.is_empty() {
                if !json {
                    println!("brief OK: {} bytes, contract present", text.len());
                }
                Ok(0)
            } else {
                if !json {
                    for violation in &violations {
                        eprintln!(
                            "{}:{}: {}",
                            violation.rule, violation.line, violation.message
                        );
                    }
                }
                Ok(2)
            }
        }
        BriefCommand::Send { worker, file, name } => {
            let client = hub.connect()?;
            let content = std::fs::read_to_string(&file)
                .map_err(|err| anyhow::anyhow!("read brief {file:?}: {err}"))?;
            let violations = lint_brief(&content);
            if !violations.is_empty() {
                for violation in &violations {
                    eprintln!(
                        "{}:{}: {}",
                        violation.rule, violation.line, violation.message
                    );
                }
                anyhow::bail!("brief failed lint; refusing to deliver");
            }
            let attachment_name = name.unwrap_or_else(|| {
                std::path::Path::new(&file)
                    .file_name()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "brief.md".into())
            });
            let value = client
                .post(
                    &format!("/v1/workers/{worker}/brief"),
                    &json!({ "content": content, "name": attachment_name }),
                )
                .await?;
            print_json(&value)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &str = "Read the diff in $WORKTREE and fix the failing test.\n\
Rules: never run deploy/ scripts. Reply on one line: DONE <sha> or BLOCKED <reason>.\n";

    #[test]
    fn clean_brief_passes() {
        assert!(lint_brief(CLEAN).is_empty());
    }

    #[test]
    fn rejects_backticks() {
        let text = format!("Run {CLEAN}\nUse `cargo test` please.\n");
        assert!(lint_brief(&text).iter().any(|v| v.rule == "backtick"));
    }

    #[test]
    fn rejects_home_paths() {
        let text = format!("{CLEAN}\nLogs are in /home/someoperator/logs.\n");
        assert!(lint_brief(&text).iter().any(|v| v.rule == "home-path"));
        // The $HOME placeholder is fine.
        let placeholder = format!("{CLEAN}\nWrite to $HOME/scratch/out.\n");
        assert!(lint_brief(&placeholder).is_empty());
    }

    #[test]
    fn rejects_missing_contract() {
        let violations = lint_brief("Just fix the bug please. No contract here.\n");
        assert!(violations.iter().any(|v| v.rule == "missing-contract"));
    }

    #[test]
    fn contract_needs_both_lines() {
        let done_only = "Do the work.\nLast line DONE <sha>.\n";
        assert!(
            lint_brief(done_only)
                .iter()
                .any(|v| v.rule == "missing-contract")
        );
    }

    #[test]
    fn denylist_hashes_are_loaded() {
        assert!(denylist().len() >= 5);
    }

    #[test]
    fn expansion_shapes_match_scanner() {
        let expanded = expansions_for("a.b.example.com");
        assert!(expanded.contains("a.b.example.com"));
        assert!(expanded.contains("example.com"));
        // Filename-looking tokens keep only the full value.
        let filename = expansions_for("src/main.rs");
        assert!(filename.contains("src/main.rs"));
        assert!(!filename.contains("main.rs"));
    }

    /// The denylist's first hash must be discoverable for the matching
    /// plaintext to ever fire — here we only prove no false positive on a
    /// normal-looking dotted host.
    #[test]
    fn ordinary_host_is_not_flagged() {
        let text = format!("{CLEAN}\nSee docs.example.org for details.\n");
        assert!(lint_brief(&text).iter().all(|v| v.rule != "private-token"));
    }
}
