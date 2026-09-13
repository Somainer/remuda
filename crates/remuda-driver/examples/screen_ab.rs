//! A/B the two attach-snapshot paths against a live `shell-pty` (D-028 §4.6).
//!
//! Spawns the same command twice — once with the emulator off (raw byte ring),
//! once with it on (synthesized repaint) — waits for the screen to settle, and
//! prints each snapshot's source, size and `alt_screen`. The bytes are written
//! to `ring.bin` / `repaint.bin` for inspection.
//!
//! ```text
//! cargo run -p remuda-driver --example screen_ab -- claude
//! AB_SETTLE=3 cargo run -p remuda-driver --example screen_ab -- python3 -u script.py
//! ```
//!
//! `AB_CWD` sets the working directory and output location (default: a
//! `remuda-screen-ab` directory under the system temp dir); `AB_SETTLE` is how
//! many seconds to wait before snapshotting (default 12 — a TUI needs time to
//! paint). Results from this harness are recorded in
//! `docs/design/evidence/native-pty-0.md`.

use remuda_driver::{Driver, ShellPtyDriver, ShellPtyOptions};
use std::time::Duration;

async fn run(label: &str, emulator: bool, script: Vec<String>) -> (usize, bool, String) {
    let dir = std::env::var("AB_CWD").map_or_else(
        |_| std::env::temp_dir().join("remuda-screen-ab"),
        std::path::PathBuf::from,
    );
    std::fs::create_dir_all(&dir).ok();
    let mut options = ShellPtyOptions::login(dir.clone());
    options.args = script;
    options.emulator = emulator;
    options.cols = 120;
    options.rows = 40;
    let driver = ShellPtyDriver::new(options);
    driver.spawn().await.expect("spawn");
    let bridge = driver.tty_bridge().await.expect("bridge");
    let remuda_driver::TtyBridge::Local(local) = bridge else {
        panic!("expected local bridge");
    };
    let settle: u64 = std::env::var("AB_SETTLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    tokio::time::sleep(Duration::from_secs(settle)).await;
    let snap = local.screen_snapshot();
    let text = String::from_utf8_lossy(&snap.bytes).into_owned();
    println!(
        "[{label}] source={} bytes={} alt_screen={}",
        snap.source.label(),
        snap.bytes.len(),
        snap.alt_screen
    );
    let _ = driver.close().await;
    (snap.bytes.len(), snap.alt_screen, text)
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let script: Vec<String> = if argv.is_empty() {
        vec!["claude".into()]
    } else {
        argv
    };
    let (ring_len, ring_alt, ring) = run("raw-ring", false, script.clone()).await;
    let (rep_len, rep_alt, rep) = run("repaint", true, script).await;
    let out = std::env::var("AB_CWD").map_or_else(
        |_| std::env::temp_dir().join("remuda-screen-ab"),
        std::path::PathBuf::from,
    );
    std::fs::write(out.join("ring.bin"), &ring).ok();
    std::fs::write(out.join("repaint.bin"), &rep).ok();
    println!("wrote {}/{{ring,repaint}}.bin", out.display());
    println!("\n--- summary ---");
    println!("raw-ring: {ring_len} bytes, alt={ring_alt}");
    println!("repaint:  {rep_len} bytes, alt={rep_alt}");
    if ring_len > 0 {
        println!(
            "repaint is {:.1}% of the ring snapshot",
            rep_len as f64 * 100.0 / ring_len as f64
        );
    }
}
