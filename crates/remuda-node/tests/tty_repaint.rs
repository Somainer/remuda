//! Attach snapshot as a synthesized repaint, behind the emulator flag
//! (D-028 §4.6, §13 P0).
//!
//! These run against a real PTY. `REMUDA_PTY_EMULATOR` selects the production
//! default, but the tests set `ShellPtyOptions::emulator` directly: the
//! workspace forbids `unsafe`, so `set_var` is unavailable, and a process-wide
//! flag would leak between tests sharing a process regardless.

use remuda_driver::shell_pty::HookConfig;
use remuda_driver::{Driver, ShellPtyDriver, ShellPtyOptions};
use remuda_node::{TtyAttach, TtyRegistry};
use remuda_protocol::InstanceId;
use std::path::PathBuf;
use std::time::Duration;

/// Paints two frames over each other, then sits still. The second frame is the
/// only thing on screen; both are in the byte ring.
const REPAINT: &str = r#"
import os, sys, time
os.write(1, b"\x1b[2J\x1b[Hfirst frame that a ring snapshot would still carry")
time.sleep(0.2)
os.write(1, b"\x1b[2J\x1b[Hsecond frame is the only thing on screen")
sys.stdout.flush()
time.sleep(30)
"#;

/// Enters the alternate screen and paints a TUI frame over real scrollback.
const ALT_SCREEN: &str = r#"
import os, sys, time
os.write(1, b"scrollback line before the tui\r\n")
os.write(1, b"\x1b[?1049h\x1b[2J\x1b[Hfullscreen tui frame")
sys.stdout.flush()
time.sleep(30)
"#;

/// Spawn a shell-pty running `script`, bridge it into a registry, and return
/// the attach once its snapshot shows `marker`.
async fn attach_when(script: &str, emulator: bool, marker: &str) -> TtyAttach {
    attach_with(script, emulator, false, marker).await
}

/// As [`attach_when`], optionally standing up the P1 hook path too, so the two
/// flags can be exercised together (D-028 §13 rule ⑤: they are orthogonal).
async fn attach_with(script: &str, emulator: bool, hooks: bool, marker: &str) -> TtyAttach {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
    options.args = vec![
        "python3".into(),
        "-u".into(),
        "-c".into(),
        script.to_owned(),
    ];
    options.emulator = emulator;
    if hooks {
        options.hooks = Some(HookConfig {
            instance_dir: dir.path().join("instance"),
            relay_binary: PathBuf::from("/nonexistent/remuda"),
            tui: remuda_driver::TuiMode::Fullscreen,
        });
    }
    let driver = ShellPtyDriver::new(options);
    driver.spawn().await.expect("spawn shell-pty");
    let bridge = driver.tty_bridge().await.expect("local bridge");

    let registry = TtyRegistry::new();
    let instance_id = InstanceId::new();
    registry
        .start(instance_id.clone(), bridge, 80, 24)
        .await
        .expect("start bridge");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(attached) = registry.attach(&instance_id).await {
            last = String::from_utf8_lossy(&attached.snapshot).into_owned();
            if last.contains(marker) {
                return attached;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("snapshot never contained {marker:?}; last was {last:?}");
}

#[tokio::test]
async fn with_the_emulator_off_the_snapshot_is_the_raw_ring_exactly_as_before() {
    let attached = attach_when(REPAINT, false, "second frame").await;
    let text = String::from_utf8_lossy(&attached.snapshot);
    assert!(
        text.contains("first frame"),
        "the ring carries overpainted frames — that is the pre-D-028 \
         behaviour P0 must preserve unchanged: {text:?}"
    );
    assert!(
        attached.alt_screen.is_none(),
        "the ring path cannot know the mode and must not claim to"
    );
    assert_eq!(
        attached.available_from,
        attached
            .next_offset
            .saturating_sub(attached.snapshot.len() as u64),
        "a ring slice keeps its place in the offset stream"
    );
}

#[tokio::test]
async fn with_the_emulator_on_the_snapshot_is_a_repaint_of_the_current_screen() {
    let attached = attach_when(REPAINT, true, "second frame").await;
    let text = String::from_utf8_lossy(&attached.snapshot);
    assert!(
        !text.contains("first frame"),
        "a repaint carries the current screen, not the history that made it \
         (§4.6): {text:?}"
    );
    assert!(
        text.starts_with("\u{1b}[!p"),
        "a repaint opens with a soft reset so the client starts clean: {text:?}"
    );
    assert_eq!(
        attached.available_from, attached.next_offset,
        "a synthesized repaint occupies no offset range of its own, so live \
         output resumes at the next unseen byte"
    );
}

#[tokio::test]
async fn an_alt_screen_session_reports_it_and_snapshots_only_the_alt_grid() {
    let attached = attach_when(ALT_SCREEN, true, "fullscreen tui frame").await;
    let text = String::from_utf8_lossy(&attached.snapshot).into_owned();
    assert!(
        attached.alt_screen == Some(true),
        "?1049 must be reported so the web stops hijacking the wheel (§4.6)"
    );
    assert!(
        !text.contains("scrollback line before the tui"),
        "a full-screen TUI has no meaningful scrollback; snapshot the alt grid \
         alone: {text:?}"
    );
    assert!(
        text.contains("\u{1b}[?1049h"),
        "the client must enter the alt buffer before the paint: {text:?}"
    );
    let json = attached.into_json().expect("attach json");
    assert_eq!(json["altScreen"], serde_json::Value::Bool(true));
    assert_eq!(
        json["representation"], "pty-bytes",
        "D-016 wire format is unchanged; altScreen is purely additive"
    );
}

#[tokio::test]
async fn a_primary_screen_session_reports_alt_screen_false() {
    let attached = attach_when(REPAINT, true, "second frame").await;
    assert_eq!(attached.alt_screen, Some(false));
    let json = attached.into_json().expect("attach json");
    assert_eq!(json["altScreen"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn the_emulator_and_hook_flags_do_not_interfere() {
    // §13 rule ⑤: the five flags are orthogonal. The hook path binds a socket
    // before the child starts and the emulator consumes bytes after the PTY
    // exists, so neither reads the other — but "should not interfere" is worth
    // an assertion rather than an argument, since both now touch `spawn_at`.
    let with_both = attach_with(ALT_SCREEN, true, true, "fullscreen tui frame").await;
    assert!(
        with_both.alt_screen == Some(true),
        "the repaint path must still report ?1049 with the hook path also up"
    );
    let text = String::from_utf8_lossy(&with_both.snapshot).into_owned();
    assert!(
        !text.contains("scrollback line before the tui"),
        "alt-grid-only snapshotting must survive the hook path: {text:?}"
    );

    // And the emulator staying off is still the raw ring when hooks are on.
    let hooks_only = attach_with(REPAINT, false, true, "second frame").await;
    assert!(
        String::from_utf8_lossy(&hooks_only.snapshot).contains("first frame"),
        "with the emulator off the snapshot is the ring, hook path or not"
    );
    assert_eq!(hooks_only.alt_screen, None);
}
