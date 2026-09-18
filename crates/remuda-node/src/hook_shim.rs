//! The Node's own executable, resolved once and pinned into the Node data dir
//! so the hook relay never names the live binary.
//!
//! On Linux a rebuild that replaces `<data>/target-node/debug/remuda` in place
//! leaves the running Node's `current_exe` reading `<path> (deleted)`. When the
//! hook command embedded that per-launch path, every hook event after the
//! rebuild died with `exec: <path> (deleted): not found`. The fix is to resolve
//! the executable at Node start, pin a private copy under
//! `<data dir>/hook-bin/<version>/remuda`, and eagerly pin it at Node start so
//! the copy captures the build the Node is running: replacing the on-disk
//! binary then breaks neither live nor newly created instances (roadmap R4
//! installed-vs-running skew).
//!
//! Pinning a ~200 MB binary can transiently fail — the source is briefly absent
//! while a rebuild unlinks the old file before linking the new one, or the
//! stripped `(deleted)` path no longer exists. That failure is **never cached**
//! and **never fatal**: the launch degrades to the resolved source path (a
//! broken hook on an instance that still launches, the pre-pin behaviour) and
//! the next launch retries the pin.
//!
//! The pinned copy is content-addressed by the build hash, so an unchanged
//! binary pins once across restarts and a replacement lands in its own version
//! directory. Old versions no live instance still references are garbage-
//! collected at Node start and after `instance.purge`; the version the running
//! Node itself pinned is never collected.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Top-level directory under the Node data dir holding pinned relay copies.
const HOOK_BIN_DIR: &str = "hook-bin";

/// Filename of the pinned relay inside each `hook-bin/<version>` directory.
const RELAY_NAME: &str = "remuda";

/// Per-instance file recording the pinned relay path a launch used, so GC can
/// tell which `hook-bin/<version>` directories a live instance still needs.
const INSTANCE_REF_FILE: &str = "hook-relay";

/// Suffix the Linux kernel appends to `/proc/self/exe` (and thus
/// `current_exe`) once the running executable's directory entry is unlinked or
/// replaced in place.
const DELETED_SUFFIX: &str = " (deleted)";

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Process-wide registry of resolved relays, keyed by `<data dir>/hook-bin`.
///
/// A [`HookRelay`] lives here rather than in `NativeDriverConfig` so the config
/// — a variant of the size-sensitive `LocalDrivers` enum — gains no field. Each
/// data dir resolves its relay exactly once; the entry is shared (`Arc`) across
/// every cloned config and driver factory, so the ~200 MB hash and copy run at
/// most once per Node.
static REGISTRY: Mutex<Option<HashMap<PathBuf, Arc<HookRelay>>>> = Mutex::new(None);

/// Resolve (once) and register the relay for `data_dir`, returning the shared
/// handle. Idempotent: a second call for the same data dir returns the first.
///
/// Cheap: `current_exe` plus, only for the `(deleted)` corner, a single `stat`.
/// The hash and copy are deferred to [`HookRelay::pin_now`] (called at Node
/// start) or the first [`HookRelay::relay_path`], whichever comes first.
pub(crate) fn ensure(data_dir: &Path) -> Arc<HookRelay> {
    let hook_bin_dir = data_dir.join(HOOK_BIN_DIR);
    let mut guard = REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner());
    let registry = guard.get_or_insert_with(HashMap::new);
    Arc::clone(
        registry
            .entry(hook_bin_dir.clone())
            .or_insert_with(|| HookRelay::from_current_exe(hook_bin_dir)),
    )
}

/// The already-registered relay for `data_dir`, resolving it if absent.
pub(crate) fn for_data_dir(data_dir: &Path) -> Arc<HookRelay> {
    ensure(data_dir)
}

/// The Node's own executable and the pinned copy the hook relay points at.
#[derive(Debug)]
pub(crate) struct HookRelay {
    /// This process's executable, de-`(deleted)`-ed. `None` when
    /// `current_exe` failed — the relay then falls back to reporting no
    /// pinned path and hooks degrade rather than the launch failing.
    source: Option<PathBuf>,
    /// `<data dir>/hook-bin`.
    hook_bin_dir: PathBuf,
    /// Memoized **successful** pin only. A pin failure is transient — the
    /// source binary can be momentarily absent while a rebuild unlinks the old
    /// file before linking the new one — so it is never cached: the next launch
    /// retries. `Some` once a pin has succeeded; pinning then never runs again.
    pinned: Mutex<Option<PathBuf>>,
}

impl HookRelay {
    /// Resolve this process's executable, stripping any `(deleted)` suffix.
    fn from_current_exe(hook_bin_dir: PathBuf) -> Arc<Self> {
        let source = match std::env::current_exe() {
            Ok(path) => Some(strip_deleted_suffix(path)),
            Err(error) => {
                tracing::warn!(%error, "cannot locate the remuda binary for hooks");
                None
            }
        };
        Arc::new(Self {
            source,
            hook_bin_dir,
            pinned: Mutex::new(None),
        })
    }

    /// The path the hook relay must reference.
    ///
    /// Prefers the Node-owned pinned copy; a successful pin is memoized so the
    /// ~200 MB hash and copy run at most once. A pin **failure is not fatal and
    /// not cached**: it degrades to the resolved source path (a broken hook on
    /// an instance that still launches, which is exactly the pre-pin
    /// behaviour), and the next launch retries the pin. `None` only when even
    /// the source could not be resolved (`current_exe` failed).
    pub(crate) fn relay_path(&self) -> Option<PathBuf> {
        if let Ok(guard) = self.pinned.lock()
            && let Some(pinned) = guard.as_ref()
        {
            return Some(pinned.clone());
        }
        let source = self.source.as_ref()?;
        match pin(source, &self.hook_bin_dir) {
            Ok(pinned) => {
                if let Ok(mut guard) = self.pinned.lock() {
                    *guard = Some(pinned.clone());
                }
                Some(pinned)
            }
            Err(error) => {
                // Do not cache: the source may reappear (a rebuild's unlink
                // window, a since-restored binary), so the next launch retries.
                // Degrade to the live source so the agent still launches.
                tracing::warn!(
                    %error,
                    source = %source.display(),
                    "could not pin the remuda hook relay; falling back to the source binary"
                );
                Some(source.clone())
            }
        }
    }

    /// Pin the relay now, if it has not been pinned yet. Called at Node start so
    /// the copy captures the running build rather than whatever lands before the
    /// first hooked launch. Best-effort: a failure is logged and retried later.
    pub(crate) fn pin_now(&self) {
        let _ = self.relay_path();
    }

    /// The pinned path if a pin has already succeeded, without forcing one.
    ///
    /// GC uses this to protect the version the running Node pinned without
    /// paying a ~200 MB hash on a Node that has not bound a hook yet.
    pub(crate) fn pinned_if_ready(&self) -> Option<PathBuf> {
        self.pinned.lock().ok().and_then(|guard| guard.clone())
    }

    /// Build a relay from an explicit source binary, for tests that must
    /// replace or delete the source without touching the running executable.
    #[cfg(test)]
    fn for_test(source: PathBuf, hook_bin_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            source: Some(source),
            hook_bin_dir,
            pinned: Mutex::new(None),
        })
    }
}

/// Register a stub relay for `data_dir`, so tests exercise the pin/GC path
/// against a controllable source rather than the ~200 MB test binary.
#[cfg(test)]
pub(crate) fn register_stub_for_test(data_dir: &Path, source: &Path) {
    let hook_bin_dir = data_dir.join(HOOK_BIN_DIR);
    let mut guard = REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner());
    let registry = guard.get_or_insert_with(HashMap::new);
    registry.insert(
        hook_bin_dir.clone(),
        HookRelay::for_test(source.to_path_buf(), hook_bin_dir),
    );
}

/// Strip a trailing `" (deleted)"` the kernel appends to a replaced executable
/// path, but only when the stripped path names an existing regular file.
///
/// A real file whose name literally ends in `" (deleted)"` (nothing at the
/// stripped path, or a non-file there) is returned untouched.
pub(crate) fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    let Some(stripped) = text.strip_suffix(DELETED_SUFFIX) else {
        return path;
    };
    let candidate = PathBuf::from(stripped);
    match fs::metadata(&candidate) {
        Ok(meta) if meta.is_file() => candidate,
        _ => path,
    }
}

/// Pin `source` into `hook-bin/<version>/remuda`, returning the pinned path.
///
/// Content-addressed by the build hash: an already-pinned version is returned
/// without touching the filesystem, so a byte-identical binary pins once across
/// restarts.
fn pin(source: &Path, hook_bin_dir: &Path) -> io::Result<PathBuf> {
    let version = version_tag(source)?;
    let version_dir = hook_bin_dir.join(&version);
    let dest = version_dir.join(RELAY_NAME);
    if is_regular_file(&dest) {
        return Ok(dest);
    }
    create_dir_private(hook_bin_dir)?;
    create_dir_private(&version_dir)?;
    let pinned = install(source, &version_dir, &dest)?;
    tracing::info!(
        source = %source.display(),
        pinned = %pinned.display(),
        "pinned remuda hook relay"
    );
    Ok(pinned)
}

/// Content hash of `source`, `sha256:`-stripped, as the version directory name.
fn version_tag(source: &Path) -> io::Result<String> {
    let digest = remuda_driver::hash_file(source)
        .map_err(|error| io::Error::other(format!("hash remuda binary: {error}")))?;
    let encoded = String::from(digest);
    Ok(encoded
        .strip_prefix("sha256:")
        .unwrap_or(&encoded)
        .to_owned())
}

/// Hard-link (falling back to copy) `source` to a temp name, then rename it to
/// `dest`. On `ETXTBSY` a relay at `dest` is mid-exec, so publish beside it
/// under a fresh path rather than fighting the busy inode.
fn install(source: &Path, version_dir: &Path, dest: &Path) -> io::Result<PathBuf> {
    let token = unique_token();
    let temp = version_dir.join(format!(".{RELAY_NAME}.{token}.part"));
    let _ = fs::remove_file(&temp);
    // A hard-link shares the source inode, which is already owner-executable
    // and cannot be rewritten in place while the Node runs it; do not chmod it,
    // that would mutate the live binary's own permissions. Only the copy
    // fallback — a distinct inode — is locked down to 0500.
    match fs::hard_link(source, &temp) {
        Ok(()) => {}
        Err(error) => {
            tracing::debug!(%error, "hook relay hard-link fell back to copy");
            copy_file(source, &temp)?;
            set_mode(&temp, 0o500)?;
        }
    }
    match fs::rename(&temp, dest) {
        Ok(()) => Ok(dest.to_path_buf()),
        Err(error) if is_etxtbsy(&error) => {
            let fresh = version_dir.join(format!("{RELAY_NAME}-{token}"));
            fs::rename(&temp, &fresh)?;
            Ok(fresh)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let mut output = OpenOptions::new().create_new(true).write(true).open(dst)?;
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    let _ = output.sync_all();
    Ok(())
}

/// Record which pinned relay a launch used, so GC can protect its version while
/// the instance is live. Best-effort: a failure here must not fail a launch.
pub(crate) fn record_instance_relay(instance_dir: &Path, pinned: &Path) {
    let path = instance_dir.join(INSTANCE_REF_FILE);
    if let Err(error) = fs::write(&path, pinned.to_string_lossy().as_bytes()) {
        tracing::warn!(%error, path = %path.display(), "could not record hook relay reference");
    }
}

/// Remove `hook-bin/<version>` directories no live instance references.
///
/// The version the running Node itself pinned (`running`) is never removed,
/// even if no instance directory names it yet. A no-op when nothing has been
/// pinned.
pub(crate) fn garbage_collect(data_dir: &Path, running: Option<&Path>) {
    let hook_bin_dir = data_dir.join(HOOK_BIN_DIR);
    if !hook_bin_dir.is_dir() {
        return;
    }
    let mut keep = referenced_versions(data_dir, &hook_bin_dir);
    if let Some(version) = running.and_then(|path| version_of(path, &hook_bin_dir)) {
        keep.insert(version);
    }
    let entries = match fs::read_dir(&hook_bin_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "could not scan hook-bin for garbage collection");
            return;
        }
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => tracing::info!(version = %name, "collected unreferenced hook relay"),
            Err(error) => {
                tracing::warn!(%error, version = %name, "could not collect hook relay")
            }
        }
    }
}

/// Versions named by the `hook-relay` reference file in each instance directory.
fn referenced_versions(data_dir: &Path, hook_bin_dir: &Path) -> HashSet<String> {
    let mut versions = HashSet::new();
    let instances = data_dir.join("instances");
    let Ok(entries) = fs::read_dir(&instances) else {
        return versions;
    };
    for entry in entries.flatten() {
        let ref_file = entry.path().join(INSTANCE_REF_FILE);
        let Ok(text) = fs::read_to_string(&ref_file) else {
            continue;
        };
        if let Some(version) = version_of(Path::new(text.trim()), hook_bin_dir) {
            versions.insert(version);
        }
    }
    versions
}

/// The `<version>` component of a `hook-bin/<version>/remuda` path, when it is
/// one of ours.
fn version_of(pinned: &Path, hook_bin_dir: &Path) -> Option<String> {
    let version_dir = pinned.parent()?;
    if version_dir.parent() != Some(hook_bin_dir) {
        return None;
    }
    version_dir.file_name()?.to_str().map(str::to_owned)
}

fn is_regular_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

fn create_dir_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)
}

fn is_etxtbsy(error: &io::Error) -> bool {
    error.kind() == ErrorKind::ExecutableFileBusy || error.raw_os_error() == Some(26)
}

fn unique_token() -> String {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{seq}", std::process::id())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Suffix rule: a `(deleted)` path whose stripped form exists as a regular
    /// file yields the stripped path; an existing file whose name really ends
    /// in that text is returned unchanged.
    #[test]
    fn deleted_suffix_is_stripped_only_when_the_real_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("remuda");
        std::fs::write(&real, b"binary").unwrap();
        let deleted = PathBuf::from(format!("{}{DELETED_SUFFIX}", real.display()));
        assert_eq!(strip_deleted_suffix(deleted), real);

        // A file whose name literally ends in " (deleted)" survives untouched.
        let literal = dir.path().join(format!("weird{DELETED_SUFFIX}"));
        std::fs::write(&literal, b"binary").unwrap();
        assert_eq!(strip_deleted_suffix(literal.clone()), literal);

        // Nothing at the stripped path: keep the original.
        let phantom = dir.path().join(format!("gone{DELETED_SUFFIX}"));
        assert_eq!(strip_deleted_suffix(phantom.clone()), phantom);
    }

    fn write_source(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        set_mode(&path, 0o755).unwrap();
        path
    }

    /// Regression: after pinning, overwriting or deleting the source leaves the
    /// pinned copy present, executable, and at a stable path.
    #[test]
    fn a_pinned_relay_survives_the_source_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let source = write_source(dir.path(), "remuda", b"#!/bin/sh\ntrue\n");
        let relay = HookRelay::for_test(source.clone(), data_dir.join(HOOK_BIN_DIR));
        let pinned = relay.relay_path().expect("pin");
        assert!(pinned.starts_with(data_dir.join(HOOK_BIN_DIR)));
        assert!(is_regular_file(&pinned));

        // Replace the source in place, then delete it entirely.
        std::fs::write(&source, b"#!/bin/sh\nfalse\n").unwrap();
        std::fs::remove_file(&source).unwrap();
        assert!(is_regular_file(&pinned), "pinned copy must survive");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                std::fs::metadata(&pinned).unwrap().permissions().mode() & 0o111 != 0,
                "pinned copy must stay executable"
            );
        }
        // Memoized: a second call is byte-identical, not a re-pin.
        assert_eq!(relay.relay_path().unwrap(), pinned);
    }

    /// GC removes the version no live instance references, keeps the referenced
    /// one, and never removes the currently running version.
    #[test]
    fn garbage_collect_keeps_referenced_and_running_versions() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let hook_bin = data_dir.join(HOOK_BIN_DIR);

        // Pin two distinct versions.
        let running_src = write_source(dir.path(), "running", b"running-bytes\n");
        let running = pin(&running_src, &hook_bin).unwrap();
        let referenced_src = write_source(dir.path(), "referenced", b"referenced-bytes\n");
        let referenced = pin(&referenced_src, &hook_bin).unwrap();
        let stale_src = write_source(dir.path(), "stale", b"stale-bytes\n");
        let stale = pin(&stale_src, &hook_bin).unwrap();

        // A live instance references only the "referenced" version.
        let instance_dir = data_dir.join("instances").join("ins_live");
        std::fs::create_dir_all(&instance_dir).unwrap();
        record_instance_relay(&instance_dir, &referenced);

        garbage_collect(&data_dir, Some(&running));

        assert!(is_regular_file(&running), "running version must survive");
        assert!(
            is_regular_file(&referenced),
            "referenced version must survive"
        );
        assert!(
            !stale.exists(),
            "the unreferenced, non-running version is collected"
        );
    }

    /// A pin failure is transient: it is never cached, degrades to the source
    /// path, and the very next call retries and succeeds once the source is back.
    #[test]
    fn a_failed_pin_falls_back_to_the_source_and_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let source = dir.path().join("remuda");
        // Source absent at first — the rebuild unlink window. hash_file fails,
        // so the pin fails.
        let relay = HookRelay::for_test(source.clone(), data_dir.join(HOOK_BIN_DIR));
        let fallback = relay
            .relay_path()
            .expect("a failed pin still yields a path");
        assert_eq!(fallback, source, "a failed pin degrades to the source path");
        assert!(
            relay.pinned_if_ready().is_none(),
            "a failed pin must not be cached"
        );

        // The source reappears (link step finished / binary restored). The next
        // call retries and now pins under hook-bin.
        write_source(dir.path(), "remuda", b"restored-bytes\n");
        let pinned = relay.relay_path().expect("retry pins");
        assert!(
            pinned.starts_with(data_dir.join(HOOK_BIN_DIR)),
            "the retry pins under hook-bin: {}",
            pinned.display()
        );
        assert_eq!(
            relay.pinned_if_ready().as_deref(),
            Some(pinned.as_path()),
            "a successful pin is now cached"
        );
    }
}
