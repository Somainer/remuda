//! Inject git SHA, rustc version, and cargo target into the `remuda` binary.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=REMUDA_GIT_SHA");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/main");

    let sha = std::env::var("REMUDA_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(git_sha);
    println!("cargo:rustc-env=REMUDA_GIT_SHA={sha}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=REMUDA_TARGET={target}");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_ver = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=REMUDA_RUSTC_VERSION={rustc_ver}");
}

fn git_sha() -> String {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let sha = output
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    if dirty && sha != "unknown" {
        format!("{sha}-dirty")
    } else {
        sha
    }
}
