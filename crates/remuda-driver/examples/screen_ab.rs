//! A/B: raw-ring vs repaint snapshot against a live shell-pty running claude.
//! Run from the worktree: `cargo run --example screen_ab -p remuda-driver`.

use remuda_driver::{Driver, ShellPtyDriver, ShellPtyOptions};
use std::time::Duration;

async fn run(label: &str, emulator: bool, script: Vec<String>) -> (usize, bool, String) {
    let dir = std::env::var("AB_CWD").unwrap_or_else(|_| "/tmp/x-p0-screen-ab/work".into());
    std::fs::create_dir_all(&dir).ok();
    let mut options = ShellPtyOptions::login(dir.clone().into());
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
    std::fs::write("/tmp/x-p0-screen-ab/ring.bin", &ring).ok();
    std::fs::write("/tmp/x-p0-screen-ab/repaint.bin", &rep).ok();
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
