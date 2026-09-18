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
//! - the `DONE <sha>` / `BLOCKED <reason>` reply contract is present;
//! - a brief that asks a worker to drive a desktop carries the terms its grant
//!   is refused without (D-045 / D-046, `docs/design/codex-cua.md` §2/§4/§5.2):
//!   the `computer-use` capability itself, the host it is granted on, and the
//!   app bundle ids the worker may answer `elicitation/create` for — and never
//!   a bypass permission posture on the same launch.

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
    /// `missing-contract` / `missing-capability` / `missing-host` /
    /// `missing-bundle-id` / `capability-bypass`).
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

// ── desktop control (D-045 / D-046) ────────────────────────────────────────
//
// A brief that sends a worker at a desktop has to carry three things, or the
// launch it implies is refused by the contract in
// `docs/design/codex-cua.md` §2/§4/§5.2:
//
//   1. the capability grant             — `capability: computer-use`
//   2. the host the grant targets       — a `hst_…` id, or a `host:` line,
//      because gate 2 of D-045 is "the host's inventory reports the cap" and
//      the operator must name the Mac explicitly (§4's non-macOS refusal).
//   3. the bundle ids the worker may answer `elicitation/create` for — D-046
//      bounds approval rights to the apps the request named.
//
// The combination of a bypass posture with desktop control is refused by §4
// (Q4), so a brief asking for both is a violation too.
//
// These rules are deliberately independent of the CLI flag implementation in
// `crates/remuda/src/cmd/dispatch.rs`: they read the brief's own text, and a
// brief that spells the terms out passes whether or not the dispatcher that
// consumes it has landed.

/// Vocabulary that means "this brief is asking for desktop control".
///
/// Both languages the skill and the contract use: the English product words,
/// and the Chinese ones a coordinator working from the skill writes.
const DESKTOP_VOCABULARY: &[&str] = &[
    "computer use",
    "computer-use",
    "computer_use",
    "iPhone Mirroring",
    "cua-repl",
    "list_apps",
    "get_app_state",
    "click on the screen",
    "操作桌面",
    "操作界面",
    "点击屏幕",
    "桌面控制",
];

/// Whether one line of prose asks for desktop control.
///
/// Case-insensitive, and a match must not run into a following word — "the
/// computer used for the build" is not desktop control. The guard only applies
/// where the term ends in an ASCII alphanumeric, so a compact Chinese term
/// (`点击屏幕…`) still matches inside a sentence.
fn desktop_vocabulary_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    DESKTOP_VOCABULARY.iter().any(|word| {
        let needle = word.to_lowercase();
        lower.match_indices(&needle).any(|(start, _)| {
            let rest = &lower[start + needle.len()..];
            !needle.ends_with(|c: char| c.is_ascii_alphanumeric())
                || !rest.starts_with(|c: char| c.is_alphanumeric())
        })
    })
}

///
/// A `capability:` declaration granting `computer-use`, with the line it sits
/// on (1-based).
///
/// The capability word and the value must share a line — that is what a
/// declaration looks like (`capability: computer-use`,
/// `capabilities: [computer-use]`, `--capability computer-use`). Prose that
/// merely mentions the capability somewhere else (a brief that says this task
/// needs *no* desktop control) is not a grant and must not be read as one.
fn capability_declared(text: &str) -> Option<usize> {
    let declaration = Regex::new(r"(?i)\bcapabilit(?:y|ies)\b").expect("regex");
    let value = Regex::new(r"(?i)\bcomputer[-_ ]use\b").expect("regex");
    text.split('\n')
        .position(|line| declaration.is_match(line) && value.is_match(line))
        .map(|index| index + 1)
}

/// A named host: a `hst_…` roster id, or a `host:` line naming one.
///
/// Gate 2 of D-045 is evaluated against a specific host, and §4 refuses a
/// non-macOS one by name, so a brief that never says which machine it means
/// cannot be dispatched to a grant.
fn host_named(text: &str) -> bool {
    let roster_id = Regex::new(r"\bhst_[A-Za-z0-9_-]+").expect("regex");
    let host_line = Regex::new(r"(?im)^\s*[-*>]?\s*host\s*[:=]\s*\S").expect("regex");
    roster_id.is_match(text) || host_line.is_match(text)
}

/// A reverse-DNS bundle id (`com.apple.TextEdit`, `com.apple.ScreenContinuity`).
///
/// Three labels minimum, none of them a single character — a bundle id is
/// `com.<vendor>.<App>`, and the shape has to exclude the prose abbreviations
/// (`e.g.`, `i.e.`) that would otherwise count as one.
fn bundle_ids(text: &str) -> Vec<String> {
    Regex::new(r"\b[a-zA-Z][a-zA-Z0-9]*(?:\.[a-zA-Z0-9][a-zA-Z0-9-]*)+\b")
        .expect("regex")
        .find_iter(text)
        .map(|found| found.as_str().to_string())
        .filter(|candidate| {
            let labels: Vec<&str> = candidate.split('.').collect();
            labels.len() >= 3 && labels.iter().all(|label| label.len() >= 2)
        })
        .collect()
}

/// The line (1-based) that asks for a permission posture skipping approvals.
fn bypass_line(text: &str) -> Option<usize> {
    let flag =
        Regex::new(r"(?i)--dangerously-(?:skip-permissions|bypass-approvals)").expect("regex");
    let word = Regex::new(r"(?i)\bbypassPermissions\b|\bbypass\s+permissions\b").expect("regex");
    text.split('\n')
        .position(|line| flag.is_match(line) || word.is_match(line))
        .map(|index| index + 1)
}

/// Line numbers stripped of fenced code blocks (1-based, kept aligned with the
/// original text so a violation can still cite the real line).
///
/// A brief that quotes a code sample mentioning `click` is not asking for
/// desktop control; only prose outside a ``` fence is instruction.
fn prose_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            out.push(String::new());
            continue;
        }
        if fenced {
            out.push(String::new());
        } else {
            out.push(line.to_string());
        }
    }
    out
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
    violations.extend(desktop_control_violations(text));
    violations
}

/// The desktop-control rule set (D-045 / D-046).
///
/// Fires only when the brief's prose — outside fenced code — uses vocabulary
/// that asks a worker to drive a desktop. Then every term the grant is refused
/// without must be present, and the bypass combination is an outright refusal.
fn desktop_control_violations(text: &str) -> Vec<Violation> {
    let prose = prose_lines(text);
    let asked_at = prose.iter().position(|line| desktop_vocabulary_line(line));
    let declared_at = capability_declared(text);
    // A brief that declares the capability is asking for it whether or not it
    // also uses the vocabulary word — the declaration *is* the request.
    let line = match (asked_at, declared_at) {
        (Some(index), _) => index + 1,
        (None, Some(index)) => index,
        (None, None) => return Vec::new(),
    };
    let mut violations = Vec::new();

    if declared_at.is_none() {
        violations.push(violation(
            line,
            "missing-capability",
            "brief asks a worker to drive a desktop but never grants the capability: add \
             `capability: computer-use` (the launch is refused without it — D-045 gate 1)",
        ));
    }
    if !host_named(text) {
        violations.push(violation(
            line,
            "missing-host",
            "brief grants desktop control without naming the host: name the macOS host \
             (`hst_…` or a `host:` line) so its inventory can be checked for the capability \
             — D-045 gate 2, and the non-macOS refusal in docs/design/codex-cua.md §4",
        ));
    }
    if bundle_ids(text).is_empty() {
        violations.push(violation(
            line,
            "missing-bundle-id",
            "brief grants desktop control without naming any app bundle id: list every app \
             the worker may act on (`com.apple.TextEdit`), because approval rights are \
             bounded to the apps the request names — D-046",
        ));
    }
    if let Some(bypass_at) = bypass_line(text) {
        violations.push(violation(
            bypass_at,
            "capability-bypass",
            "brief asks for computer-use together with a bypassed permission posture: the \
             combination is refused (unattended desktop control plus skipped tool approvals \
             has no recovery path — D-045 §4 / Q4). Ask for one of the two, not both",
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
    /// Validate a brief file (backticks, home paths, private tokens, contract,
    /// desktop-control terms).
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

    // ── desktop control (D-045 / D-046) ────────────────────────────────────

    /// The grant as a brief writes it: all three terms, on one machine.
    const GRANTED: &str = "Driver: use computer-use to read the note in TextEdit.\n\
        capability: computer-use\n\
        host: hst_mac-mini\n\
        Approved apps (bundle ids): com.apple.TextEdit\n\
        Do not act on any other app.\n";

    #[test]
    fn desktop_vocabulary_without_capability_is_rejected() {
        let text = format!("{CLEAN}\nUse computer use to read the note on screen.\n");
        let violations = lint_brief(&text);
        let found = violations
            .iter()
            .find(|v| v.rule == "missing-capability")
            .expect("desktop vocabulary must require the capability");
        assert!(found.message.contains("computer-use"), "{}", found.message);
        // The vocabulary line is what the violation points at.
        assert_eq!(
            found.line,
            text.lines()
                .position(|l| l.contains("computer use"))
                .unwrap()
                + 1
        );
    }

    #[test]
    fn desktop_vocabulary_with_capability_is_clean() {
        let text = format!("{CLEAN}\n{GRANTED}");
        assert!(lint_brief(&text).is_empty(), "{:?}", lint_brief(&text));
    }

    #[test]
    fn capability_without_host_or_bundle_id_is_rejected() {
        let text =
            format!("{CLEAN}\nUse computer-use to read the note.\ncapability: computer-use\n");
        let rules: Vec<&str> = lint_brief(&text).iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"missing-host"), "{rules:?}");
        assert!(rules.contains(&"missing-bundle-id"), "{rules:?}");
        assert!(!rules.contains(&"missing-capability"), "{rules:?}");
    }

    #[test]
    fn bypass_with_computer_use_is_rejected() {
        let text = format!("{CLEAN}\n{GRANTED}Launch with --dangerously-skip-permissions.\n");
        let violations = lint_brief(&text);
        let found = violations
            .iter()
            .find(|v| v.rule == "capability-bypass")
            .expect("bypass plus computer-use must be refused");
        assert!(found.message.contains("D-045"), "{}", found.message);
    }

    #[test]
    fn bypass_word_forms_are_caught() {
        for phrase in ["bypassPermissions", "bypass permissions"] {
            let text = format!("{CLEAN}\n{GRANTED}Run under {phrase} for this one.\n");
            assert!(
                lint_brief(&text)
                    .iter()
                    .any(|v| v.rule == "capability-bypass"),
                "{phrase}"
            );
        }
    }

    /// A code sample is not an instruction: fenced blocks are invisible to the
    /// vocabulary probe, so a brief about a UI *codebase* stays clean.
    #[test]
    fn fenced_code_mentioning_click_is_not_desktop_control() {
        let text = format!(
            "{CLEAN}\nFix the handler that fires when the user clicks a row.\n\n\
             ```js\n// click on the screen element\n\
             element.addEventListener(\"click\", onClick);\n```\n"
        );
        let rules: Vec<&str> = lint_brief(&text).iter().map(|v| v.rule).collect();
        assert!(
            !rules
                .iter()
                .any(|rule| rule.starts_with("missing-") || *rule == "capability-bypass"),
            "{rules:?}"
        );
    }

    /// An unrelated brief with no desktop vocabulary is untouched by the rule
    /// even though it names neither host nor bundle id.
    #[test]
    fn ordinary_brief_needs_no_capability_terms() {
        let violations = lint_brief(CLEAN);
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn chinese_desktop_vocabulary_is_caught() {
        let text = format!("{CLEAN}\n请点击屏幕上的按钮完成任务。\n");
        assert!(
            lint_brief(&text)
                .iter()
                .any(|v| v.rule == "missing-capability"),
            "{:?}",
            lint_brief(&text)
        );
    }

    /// Vocabulary matching is case-insensitive and does not run into the next
    /// word: "the computer used for the build" is not desktop control.
    #[test]
    fn vocabulary_is_case_insensitive_but_bounded() {
        assert!(desktop_vocabulary_line("Use Computer Use to read it."));
        assert!(desktop_vocabulary_line("drive the desktop with CUA-REPL"));
        assert!(!desktop_vocabulary_line("the computer used for the build"));
        // A compact Chinese term still matches inside a sentence.
        assert!(desktop_vocabulary_line("请点击屏幕上的按钮"));
    }

    #[test]
    fn bundle_id_shape_excludes_prose_abbreviations() {
        assert!(bundle_ids("use com.apple.TextEdit only").contains(&"com.apple.TextEdit".into()));
        // Three labels minimum, no single-character labels.
        assert!(bundle_ids("see e.g. the docs").is_empty());
        assert!(bundle_ids("see a.b for details").is_empty());
    }

    #[test]
    fn host_roster_id_or_host_line_both_count() {
        assert!(host_named("dispatch to hst_abc123"));
        assert!(host_named("host: mac-mini.local"));
        assert!(!host_named("run it on whatever host is free"));
    }

    /// Merely naming the capability in prose is not declaring it: a brief that
    /// tells the worker the capability is not on offer stays clean.
    #[test]
    fn prose_mention_of_the_capability_is_not_a_grant() {
        // No vocabulary word and no declaration line, but the capability is
        // named. This must not read as a grant.
        let text =
            format!("{CLEAN}\nThis session holds no capability; do not ask for computer-use.\n");
        let rules: Vec<&str> = lint_brief(&text).iter().map(|v| v.rule).collect();
        assert!(!rules.contains(&"missing-capability"), "{rules:?}");
    }

    /// A declaration is itself the request: no vocabulary word needed.
    #[test]
    fn capability_declaration_alone_triggers_the_rule() {
        let text = format!("{CLEAN}\ncapability: computer-use\n");
        let rules: Vec<&str> = lint_brief(&text).iter().map(|v| v.rule).collect();
        assert!(rules.contains(&"missing-host"), "{rules:?}");
        assert!(!rules.contains(&"missing-capability"), "{rules:?}");
    }
}
