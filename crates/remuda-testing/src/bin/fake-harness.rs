//! Deterministic TUI stand-in for `claude` / `codex` / `grok` (D-028 P7).
//!
//! See `docs/design/testing-fake-harness.md` for the scenario format, flags,
//! and the explicit non-goals.

use std::path::PathBuf;

use clap::Parser;
use remuda_testing::fake_harness::{Dialect, DialectVersion, Options, run};

/// Script-driven fake agent harness for PTY tests.
#[derive(Parser, Debug)]
#[command(name = "fake-harness", version, disable_help_subcommand = true)]
struct Args {
    /// Screen/artifact dialect.
    ///
    /// Defaults to claude because the real materializer never passes a
    /// `--kind` flag — it selects the binary instead; the fake is one binary
    /// emulating all three dialects, so the explicit flag exists only for the
    /// fake-harness test harness.
    #[arg(long, value_parser = parse_kind, default_value = "claude")]
    kind: Dialect,
    /// Scenario file (.json / .yaml / .yml); built-in default when omitted.
    #[arg(long)]
    script: Option<PathBuf>,
    /// Claude-style settings overlay containing hooks.
    #[arg(long)]
    settings: Option<PathBuf>,
    /// Accept the launch shim's source selection; only --settings is loaded.
    /// The fake does not emulate claude's config-source layering, so this is
    /// accepted and ignored.
    #[arg(long = "setting-sources", allow_hyphen_values = true, hide = true)]
    _setting_sources: Option<String>,
    /// Harness home (CLAUDE_CONFIG_DIR / CODEX_HOME / GROK_HOME equivalent).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Absorbed from the materializer argv for bypass-permissions launches.
    /// The fake drives approval itself from the scenario's `approval` field.
    #[arg(long = "allow-dangerously-skip-permissions", hide = true)]
    _skip_permissions: bool,
    /// Legacy spelling on older builds; ignored the same way.
    #[arg(long = "dangerously-skip-permissions", hide = true)]
    _skip_permissions_legacy: bool,
    /// Absorb any remaining unknown trailing arguments (forward-compatible
    /// with future materializer flags).
    #[arg(last = true, allow_hyphen_values = true, hide = true)]
    _ignored: Vec<String>,
    /// Working directory the session reports.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Set the session id.
    #[arg(long)]
    session_id: Option<String>,
    /// Continue an existing artifact set.
    #[arg(long)]
    resume: Option<String>,
    /// Model label.
    #[arg(long)]
    model: Option<String>,
    /// Show the first-run trust-directory dialog.
    #[arg(long)]
    trust_dialog: bool,
    /// Do not enter the alt screen (mirrors codex/grok --no-alt-screen).
    #[arg(long)]
    no_alt_screen: bool,
    /// Fallback terminal width.
    #[arg(long, default_value_t = 80)]
    cols: u16,
    /// Fallback terminal height.
    #[arg(long, default_value_t = 24)]
    rows: u16,
    /// Pin the deterministic clock epoch (Unix milliseconds).
    #[arg(long)]
    epoch_ms: Option<i64>,
    /// Screen dialect version: `legacy` (default) or `modern` (claude 2.1.270).
    #[arg(long, default_value = "legacy", value_parser = DialectVersion::parse)]
    dialect_version: DialectVersion,
    /// Append semantic debug events to this JSONL file.
    #[arg(long)]
    events_out: Option<PathBuf>,
}

fn parse_kind(value: &str) -> Result<Dialect, String> {
    Dialect::parse(value)
}

fn main() {
    let args = Args::parse();
    let options = Options {
        kind: args.kind,
        script_path: args.script,
        settings: args.settings,
        home: args.home,
        cwd: args.cwd,
        session_id: args.session_id,
        resume: args.resume,
        model: args.model,
        trust_dialog: args.trust_dialog,
        no_alt_screen: args.no_alt_screen,
        cols: args.cols,
        rows: args.rows,
        epoch_ms: args.epoch_ms,
        dialect_version: args.dialect_version,
        events_path: args.events_out,
    };
    match run(options) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("fake-harness: {err}");
            std::process::exit(1);
        }
    }
}
