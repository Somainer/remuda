//! Copy `web/dist` (feature `embed-web`) or the crate 404 page into OUT_DIR.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("web-dist");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).expect("web-dist out dir");

    let workspace_dist = manifest.join("../../web/dist");
    let fallback = manifest.join("fallback-web/index.html");
    let embed = env::var("CARGO_FEATURE_EMBED_WEB").is_ok();

    if embed && workspace_dist.join("index.html").is_file() {
        copy_dir(&workspace_dist, &out);
    } else {
        fs::copy(&fallback, out.join("index.html")).expect("fallback web page");
    }

    println!("cargo:rerun-if-changed=fallback-web/index.html");
    println!("cargo:rerun-if-changed=../../web/dist");
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir");
    for entry in fs::read_dir(src).expect("read dist") {
        let entry = entry.expect("dirent");
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("ftype").is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).expect("copy file");
        }
    }
}
