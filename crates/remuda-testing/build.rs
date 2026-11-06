//! Build stamp for the remuda-testing binaries (fake-herdr, fake-claude, …).
//!
//! Tests locate these binaries in a target dir that can outlive the checkout
//! that built them (the landing gate builds every candidate in a throwaway
//! worktree inside one shared target dir), and nothing rebuilds a binary that
//! is already on disk. The stamp names the tree a build came from: the
//! checkout path plus a hash of every source the binaries are compiled from
//! (this crate's `src/` and `fixtures/`, the `src/` of each workspace crate it
//! depends on, transitively, their manifests, and `Cargo.lock`). Every binary
//! embeds it, and `ensure_workspace_bin` reuses only a binary whose stamp
//! equals its own (see `bin_locator.rs`).

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest.parent().expect("crates dir").to_path_buf();
    let workspace = crates_dir.parent().expect("workspace root").to_path_buf();

    let mut crates = BTreeSet::new();
    collect_closure(&crates_dir, "remuda-testing", &mut crates);

    let mut inputs: Vec<PathBuf> = vec![workspace.join("Cargo.toml"), workspace.join("Cargo.lock")];
    for name in &crates {
        let dir = crates_dir.join(name);
        inputs.push(dir.join("Cargo.toml"));
        inputs.push(dir.join("build.rs"));
        inputs.push(dir.join("src"));
    }
    inputs.push(manifest.join("fixtures"));

    let mut files = Vec::new();
    for input in &inputs {
        // A missing path would make cargo rerun this script on every build.
        if input.exists() {
            println!("cargo:rerun-if-changed={}", input.display());
        }
        walk(input, &mut files);
    }
    files.sort();

    let mut hash = Sha256::new();
    let canonical = manifest.canonicalize().unwrap_or(manifest.clone());
    hash.update(canonical.to_string_lossy().as_bytes());
    hash.update([0]);
    for file in &files {
        let relative = file.strip_prefix(&workspace).unwrap_or(file);
        hash.update(relative.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update(fs::read(file).unwrap_or_default());
        hash.update([0]);
    }
    let stamp: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("cargo:rustc-env=REMUDA_TESTING_BUILD_STAMP={stamp}");
}

/// `name` and every `remuda-*` crate its normal (not dev/build) dependency
/// sections name, transitively.
fn collect_closure(crates_dir: &Path, name: &str, seen: &mut BTreeSet<String>) {
    if !seen.insert(name.to_owned()) {
        return;
    }
    let Ok(manifest) = fs::read_to_string(crates_dir.join(name).join("Cargo.toml")) else {
        return;
    };
    let mut in_deps = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line.contains("dependencies")
                && !line.contains("dev-dependencies")
                && !line.contains("build-dependencies");
            continue;
        }
        if !in_deps || !line.starts_with("remuda-") {
            continue;
        }
        let dep: String = line
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if crates_dir.join(&dep).join("Cargo.toml").is_file() {
            collect_closure(crates_dir, &dep, seen);
        }
    }
}

fn walk(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        walk(&entry.path(), out);
    }
}
