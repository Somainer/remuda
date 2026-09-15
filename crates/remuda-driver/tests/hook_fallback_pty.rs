//! `respond_interaction` over a real PTY: hook first, screen once, never a
//! false success (D-028 §4.4, §14 risk 1).
//!
//! The unit tests in `shell_pty::answer` decide *which keys* from a grid, and
//! `hook_answer` guards the budget. What neither covers is the part that only
//! a real PTY shows: that the driver reads the live screen, writes the keys
//! into it, waits for the dialog to actually go away, and refuses to call the
//! decision applied when it does not.
//!
//! The peer is a small script rendering the approval dialog measured from
//! claude 2.1.221, so the assertions are about bytes a real TUI would have
//! seen rather than about a mock.
#![cfg(unix)]

use remuda_driver::shell_pty::{ShellPtyDriver, ShellPtyOptions};
use remuda_driver::tty::TtyBridge;
use remuda_driver::{Driver, DriverAck};
use remuda_protocol::{ApprovalAnswer, Digest, DispatchState, InteractionAnswer, InteractionId};
use std::time::Duration;

/// The dialog claude renders for a `Write` (evidence native-pty-5 §2), which
/// repaints into a non-approval screen once a key arrives.
///
/// Reading a byte is what makes this a fair test of the fallback: the driver
/// has to have actually written into the PTY for the screen to change. The
/// erase is what a real TUI does — without it the dialog text stays in the
/// grid and `dialog_cleared` would correctly refuse to confirm.
const DIALOG_THEN_CLEAR: &str = r#"
printf 'Do you want to create probe.txt?\r\n'
printf '❯ 1. Yes\r\n'
printf '  2. Yes, allow all edits during this session (shift+tab)\r\n'
printf '  3. No\r\n'
dd bs=1 count=1 2>/dev/null >/dev/null
printf '\033[2J\033[H'
printf 'Wrote 1 line to probe.txt\r\n'
sleep 30
"#;

/// The same dialog, but nothing clears it however many keys arrive — a session
/// that ignores the answer.
const DIALOG_FOREVER: &str = r#"
printf 'Do you want to create probe.txt?\r\n'
printf '❯ 1. Yes\r\n'
printf '  2. Yes, allow all edits during this session (shift+tab)\r\n'
printf '  3. No\r\n'
sleep 30
"#;

fn driver(script: &str) -> (ShellPtyDriver, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut options = ShellPtyOptions::login(dir.path().to_path_buf());
    options.args = vec!["/bin/sh".into(), "-c".into(), script.to_owned()];
    // The emulator is what turns raw bytes into the grid the matchers read.
    options.emulator = true;
    (ShellPtyDriver::new(options), dir)
}

fn approval(option: &str) -> InteractionAnswer {
    InteractionAnswer::Approval(Box::new(ApprovalAnswer {
        option_id: option.to_owned(),
        input_digest: Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap(),
    }))
}

/// Wait until `needle` has been painted, so the test is not racing the shell.
async fn await_screen(driver: &ShellPtyDriver, needle: &str) {
    for _ in 0..100 {
        if screen(driver).await.contains(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{needle}` never appeared on the pty");
}

/// The PTY's current screen as text.
async fn screen(driver: &ShellPtyDriver) -> String {
    match Driver::tty_bridge(driver).await {
        Some(TtyBridge::Local(pty)) => String::from_utf8_lossy(&pty.snapshot()).into_owned(),
        _ => String::new(),
    }
}

/// Whether the driver reported the decision as having reached the agent.
fn applied(ack: &DriverAck) -> bool {
    ack.dispatch != DispatchState::NotDispatched
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ignored_decision_is_answered_on_screen_once_and_confirmed() {
    let (driver, _dir) = driver(DIALOG_THEN_CLEAR);
    let _handle = driver.spawn().await.expect("pty spawns");
    await_screen(&driver, "Do you want to create").await;

    // No hook is parked for this id, so this is the ignored/abandoned path the
    // fallback exists for.
    let id = InteractionId::new();
    let ack = Driver::respond_interaction(&driver, id.clone(), approval("allow-once")).await;
    let ack = ack.expect("responding must not error");
    assert!(
        applied(&ack),
        "the dialog cleared after the keys, so the decision was applied"
    );

    // The second attempt is refused: a replayed Enter answers whatever prompt
    // is showing now, which may be a different question entirely (D-022).
    let again = Driver::respond_interaction(&driver, id, approval("allow-once"))
        .await
        .expect("a refused replay is not an error");
    assert!(
        !applied(&again),
        "the one screen-key attempt per interaction must not be replayed"
    );
    let _ = Driver::close(&driver).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dialog_that_never_clears_is_not_reported_as_applied() {
    // §14 risk 1: keys written is not the same as decision taken. Saying
    // "applied" here is what leaves a user believing they approved something.
    let (driver, _dir) = driver(DIALOG_FOREVER);
    let _handle = driver.spawn().await.expect("pty spawns");
    await_screen(&driver, "Do you want to create").await;

    let ack = Driver::respond_interaction(&driver, InteractionId::new(), approval("allow-once"))
        .await
        .expect("responding must not error");
    assert!(
        !applied(&ack),
        "an unconfirmed screen answer must not be reported as applied"
    );
    let _ = Driver::close(&driver).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_screen_with_no_approval_on_it_is_left_alone() {
    // Pressing a guessed key into an agent TUI is how a fallback becomes an
    // unintended answer; a model picker and an approval look alike (§10 ③).
    let (driver, _dir) =
        driver("printf 'Select a model\\r\\n  1. Opus\\r\\n  2. Sonnet\\r\\n'; sleep 30");
    let _handle = driver.spawn().await.expect("pty spawns");
    await_screen(&driver, "Select a model").await;

    let ack = Driver::respond_interaction(&driver, InteractionId::new(), approval("allow-once"))
        .await
        .expect("responding must not error");
    assert!(
        !applied(&ack),
        "nothing may be pressed when no approval dialog is on screen"
    );
    let _ = Driver::close(&driver).await;
}
