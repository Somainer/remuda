//! Embed build identity; SOURCE_DATE_EPOCH supports reproducible release builds.

use std::{env, path::PathBuf, process::Command};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

fn main() {
    for key in [
        "REMUDA_GIT_SHA",
        "REMUDA_BUILD_DATE",
        "SOURCE_DATE_EPOCH",
        "TARGET",
        "RUSTC",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"));
    let root = manifest.join("../..");
    // Resolve gitdir files and worktree common refs instead of assuming .git is a directory.
    for reference in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                reference,
            ],
        ) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = git(&root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &reference,
            ],
        )
    {
        println!("cargo:rerun-if-changed={path}");
    }
    let sha = environment("REMUDA_GIT_SHA")
        .or_else(|| git(&root, &["rev-parse", "--verify", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    emit("REMUDA_GIT_SHA", &sha);
    emit(
        "REMUDA_TARGET",
        &env::var("TARGET").expect("Cargo compilation target"),
    );
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    emit("REMUDA_RUSTC_VERSION", &version);
    let date = if let Some(value) = environment("REMUDA_BUILD_DATE") {
        OffsetDateTime::parse(&value, &Rfc3339)
            .expect("REMUDA_BUILD_DATE must be RFC3339")
            .to_offset(time::UtcOffset::UTC)
    } else if let Some(value) = environment("SOURCE_DATE_EPOCH") {
        let seconds = value
            .parse::<i64>()
            .expect("SOURCE_DATE_EPOCH must be Unix seconds");
        assert!(seconds >= 0, "SOURCE_DATE_EPOCH must not be negative");
        OffsetDateTime::from_unix_timestamp(seconds)
            .expect("SOURCE_DATE_EPOCH must be representable")
    } else {
        OffsetDateTime::now_utc()
    };
    let date = date
        .replace_nanosecond(0)
        .expect("zero nanoseconds")
        .format(&Rfc3339)
        .expect("RFC3339 build date");
    emit("REMUDA_BUILD_DATE", &date);
}

fn environment(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => {
            assert!(!value.is_empty(), "{key} must not be empty");
            Some(value)
        }
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => panic!("{key} must be UTF-8"),
    }
}

fn emit(key: &str, value: &str) {
    assert!(
        !value.contains(['\n', '\r', '\0']),
        "{key} must be a single line"
    );
    println!("cargo:rustc-env={key}={value}");
}

fn git(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
