//! Golden plaintext grids for each dialect/state/size.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p remuda-testing --test fake_harness golden`.

#![allow(clippy::missing_panics_doc)]

use remuda_testing::fake_harness::screen::{
    ApprovalView, Dialect, ScreenMode, View, WorkingPhase, render_grid,
};
use std::path::PathBuf;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake-harness/golden")
}

fn canonical_view(dialect: Dialect, state: &str) -> View {
    let model = match dialect {
        Dialect::Claude => "claude-opus-5",
        Dialect::Codex => "gpt-5.4",
        Dialect::Grok => "Local spike",
    };
    let mut view = View::new(dialect, model.into(), "wt".into());
    match state {
        "idle" => {}
        "trust" => {
            view.mode = ScreenMode::Trust;
        }
        "approval" => {
            view.transcript = vec!["RUN_TOOL".into()];
            view.mode = ScreenMode::Approval;
            view.running_tool = Some("Bash".into());
            view.approval = Some(ApprovalView {
                tool: "Bash".into(),
                command: "printf SPIKE_TOOL_OK > spike-result.txt".into(),
                reason: Some("Write the fixed probe marker.".into()),
                selected: 0,
                choices: remuda_testing::fake_harness::screen::default_choices(dialect),
            });
        }
        "working" => {
            view.transcript = vec![
                "SLOW_QUEUE".into(),
                match dialect {
                    Dialect::Claude => "⏺ Bash(sleep 20)",
                    Dialect::Codex => "$ sleep 20",
                    Dialect::Grok => "◆ Sleep for 20 seconds",
                }
                .into(),
            ];
            view.mode = ScreenMode::Working;
            view.phase = Some(WorkingPhase::Running);
            view.elapsed_secs = 2;
            view.running_tool = Some("Bash".into());
            view.draft = "QUEUED_FOLLOWUP".into();
            match dialect {
                Dialect::Claude => view.queued = vec![],
                Dialect::Codex => view.queued = vec![],
                Dialect::Grok => view.queued = vec!["QUEUED_FOLLOWUP".into()],
            }
        }
        other => panic!("unknown canonical state {other}"),
    }
    view
}

#[test]
fn golden_screens_match() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let cases = [
        (Dialect::Claude, "idle", 80, 24),
        (Dialect::Claude, "working", 80, 24),
        (Dialect::Claude, "approval", 80, 24),
        (Dialect::Claude, "trust", 80, 24),
        (Dialect::Codex, "idle", 80, 24),
        (Dialect::Codex, "working", 80, 24),
        (Dialect::Codex, "approval", 80, 24),
        (Dialect::Codex, "trust", 80, 24),
        (Dialect::Grok, "idle", 80, 24),
        (Dialect::Grok, "working", 80, 24),
        (Dialect::Grok, "approval", 80, 24),
        (Dialect::Grok, "trust", 80, 24),
        (Dialect::Claude, "working", 40, 20),
        (Dialect::Codex, "working", 40, 20),
        (Dialect::Grok, "working", 40, 20),
        (Dialect::Claude, "idle", 40, 20),
        (Dialect::Codex, "idle", 40, 20),
        (Dialect::Grok, "idle", 40, 20),
    ];
    for (dialect, state, cols, rows) in cases {
        let view = canonical_view(dialect, state);
        let grid = render_grid(&view, cols, rows);
        let rendered = format!("{}\n", grid.join("\n"));
        let name = format!(
            "{}-{state}-{cols}x{rows}.txt",
            match dialect {
                Dialect::Claude => "claude",
                Dialect::Codex => "codex",
                Dialect::Grok => "grok",
            }
        );
        let path = golden_dir().join(name);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, rendered).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("missing golden {}: {err}", path.display()));
        assert_eq!(
            expected,
            rendered,
            "golden mismatch for {} (set UPDATE_GOLDEN=1 to refresh)",
            path.display()
        );
    }
}
