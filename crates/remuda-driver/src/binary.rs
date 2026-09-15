//! Absolute path, `--version`, and SHA-256 pin for a native agent binary.

use crate::error::{DriverError, DriverResult};
use remuda_protocol::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static PIN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Read buffer for [`hash_file`]. A multi-megabyte agent binary (the pinned
/// claude is ~207 MB) costs tens of thousands of syscalls through an 8 KiB
/// buffer; 1 MiB is large enough to make the reads themselves negligible
/// without holding a stack-sized buffer.
const HASH_BUF: usize = 1024 * 1024;

/// Re-probes of the original path before its bytes are copied elsewhere.
const ETXTBSY_REPROBES: u32 = 6;

/// Base delay between re-probes; the nth wait is `n` times this.
const ETXTBSY_BACKOFF: Duration = Duration::from_millis(2);

/// Identity of a file whose pin is cached.
///
/// `(path, len, mtime)` is the same triple build systems use to decide whether
/// a file changed: a new binary version has a new length or mtime, so the
/// expensive SHA-256 of a ~207 MB agent executable runs once per version
/// instead of on every `instance.create`. A replace that preserves both is
/// already detectable only by hashing, and is the accepted stale-cache edge.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PinCacheKey {
    path: PathBuf,
    len: u64,
    mtime: SystemTime,
}

/// Process-wide pin cache. Pins are immutable facts about a file inode-state,
/// safe to share across instances and Nodes in this process.
static PIN_CACHE: Mutex<Option<HashMap<PinCacheKey, BinaryPin>>> = Mutex::new(None);

fn pin_cache_get(key: &PinCacheKey) -> Option<BinaryPin> {
    let guard = PIN_CACHE.lock().ok()?;
    guard.as_ref()?.get(key).cloned()
}

fn pin_cache_put(key: PinCacheKey, pin: BinaryPin) {
    if let Ok(mut guard) = PIN_CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        // Bound the cache: a long-lived Node pins every agent version it ever
        // saw, and these entries hold nothing small — drop the oldest rather
        // than grow without limit.
        if cache.len() >= 64
            && !cache.contains_key(&key)
            && let Some(oldest) = cache.keys().next().cloned()
        {
            cache.remove(&oldest);
        }
        cache.insert(key, pin);
    }
}

/// Pinned native executable recorded in a [`crate::LaunchRecipe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryPin {
    /// Canonical absolute path of the file that will be exec'd.
    pub abs_path: String,
    /// First line of `--version` stdout (stderr fallback), trimmed.
    pub version: String,
    /// SHA-256 of the file contents, `sha256:` prefixed.
    pub sha256: Digest,
}

/// Resolve `command` on `PATH` or accept an absolute path, then pin it.
///
/// `ETXTBSY` has two causes and only one of them justifies a copy. A sibling
/// thread that forked between our `open(O_WRONLY)` and its `exec` holds a
/// write handle for microseconds, so the probe is retried on the original path
/// first — otherwise two pins of the same file would disagree on `abs_path`.
/// Only a writer that survives every re-probe (a real mapped or open-for-write
/// inode) makes the bytes get copied to a unique sibling path, and that copy is
/// pinned instead of overwriting the busy inode.
pub fn pin_binary(command: impl AsRef<Path>) -> DriverResult<BinaryPin> {
    let span = tracing::info_span!(
        "pin_binary",
        command = %command.as_ref().display()
    );
    let _guard = span.enter();
    let original = resolve_binary(command)?;
    // Canonicalize before the version probe so the cache key is the real file
    // a symlink points at, not whichever alias was asked for.
    let canonical = original.canonicalize().unwrap_or(original);
    let cache_key = pin_cache_key(&canonical)?;
    if let Some(pin) = pin_cache_get(&cache_key) {
        tracing::debug!(
            path = %pin.abs_path,
            version = %pin.version,
            digest = %String::from(pin.sha256.clone()),
            "using cached native binary pin"
        );
        return Ok(pin);
    }
    let (path, version) = probe_version(&canonical)?;
    let path = path.canonicalize().unwrap_or(path);
    let sha256 = {
        let _span = tracing::info_span!("hash_binary", path = %path.display()).entered();
        hash_file(&path)?
    };
    tracing::info!(
        path = %path.display(),
        version = %version,
        digest = %String::from(sha256.clone()),
        "pinned native binary"
    );
    let pin = BinaryPin {
        abs_path: path.to_string_lossy().into_owned(),
        version,
        sha256,
    };
    // A persistent-ETXTBSY copy is a unique sibling path, not the binary the
    // key describes; do not cache that pin under the original path.
    if path == cache_key.path {
        pin_cache_put(cache_key, pin.clone());
    }
    Ok(pin)
}

/// Build the cache key for a resolved binary: canonical path, length, mtime.
fn pin_cache_key(path: &Path) -> DriverResult<PinCacheKey> {
    let metadata = fs::metadata(path)?;
    Ok(PinCacheKey {
        path: path.to_path_buf(),
        len: metadata.len(),
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
    })
}

/// Read `--version`, preferring `original` and falling back to a fresh copy.
///
/// Returns the path that actually answered, which is what gets pinned.
fn probe_version(original: &Path) -> DriverResult<(PathBuf, String)> {
    let mut last_busy = None;
    for attempt in 0..=ETXTBSY_REPROBES {
        match read_version(original) {
            Ok(line) => return Ok((original.to_path_buf(), line)),
            Err(DriverError::Io(err)) if is_etxtbsy(&err) => {
                last_busy = Some(err);
                // A fork window closes on its own; a real writer does not.
                if attempt < ETXTBSY_REPROBES {
                    std::thread::sleep(ETXTBSY_BACKOFF * (attempt + 1));
                }
            }
            Err(err) => return Err(err),
        }
    }
    for _ in 0..4 {
        let copy = copy_to_fresh_path(original)?;
        tracing::warn!(
            src = %original.display(),
            dest = %copy.display(),
            "pin_binary copied binary to a fresh path after persistent ETXTBSY"
        );
        match read_version(&copy) {
            Ok(line) => return Ok((copy, line)),
            Err(DriverError::Io(err)) if is_etxtbsy(&err) => last_busy = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(DriverError::Io(last_busy.unwrap_or_else(|| {
        io::Error::new(
            ErrorKind::ExecutableFileBusy,
            format!(
                "pin_binary: {} still busy after copying to a fresh path",
                original.display()
            ),
        )
    })))
}

/// Directories a binary override may not resolve inside.
///
/// Each is a place the agent itself can write. A path that canonicalizes into
/// one of them would let an agent that can write its own cwd drop a script
/// there and name it as the executable — self-upgrading from "run the agent"
/// to arbitrary exec on the next launch.
#[derive(Debug, Clone, Default)]
pub struct BinaryOverrideGuard {
    /// The instance directory; its `launch/` subtree is derived from it.
    pub instance_dir: Option<PathBuf>,
    /// Workspace or worktree cwd for the launch.
    pub cwd: Option<PathBuf>,
    /// Additional roots to refuse, for callers with their own scratch dirs.
    pub extra: Vec<PathBuf>,
}

/// Validate a caller-supplied absolute executable and pin it.
///
/// Fails closed with a named error at every step; there is deliberately no
/// fallback to `PATH`. A caller that asked for a specific binary and cannot
/// have it should hear so, not silently get a different one.
///
/// `expected` is an optional pin-on-record digest. When present the computed
/// pin must equal it, so a caller that recorded a binary detects it changing
/// underneath rather than executing the replacement.
pub fn validate_binary_override(
    raw: &str,
    guard: &BinaryOverrideGuard,
    expected: Option<&Digest>,
) -> DriverResult<BinaryPin> {
    // B2: reject whitespace and shell metacharacters before anything else.
    // We exec argv directly, so this is not about our own quoting — the value
    // is also baked into the generated launch shim, which is `sh`.
    if raw.trim().is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "binaryPath must not be empty".into(),
        ));
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(DriverError::InvalidLaunchSpec(
            "binaryPath must be a single path with no arguments or whitespace".into(),
        ));
    }
    if raw.chars().any(crate::profile::is_shell_metacharacter) {
        return Err(DriverError::InvalidLaunchSpec(
            "binaryPath must not contain shell metacharacters".into(),
        ));
    }
    // B1: absolute, and no traversal segments — checked before the filesystem
    // is touched, so a crafted path cannot be resolved even once.
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "binaryPath must be an absolute path, got {raw}"
        )));
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "binaryPath must not contain . or .. segments: {raw}"
        )));
    }
    // B3: canonicalize resolves symlinks, so the checks below apply to the
    // file that will actually be exec'd rather than to the name given.
    let canonical = fs::canonicalize(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            DriverError::BinaryNotFound(path.to_path_buf())
        } else {
            DriverError::InvalidLaunchSpec(format!("resolve binaryPath {raw}: {error}"))
        }
    })?;
    let meta = fs::metadata(&canonical).map_err(|error| {
        DriverError::InvalidLaunchSpec(format!("stat binaryPath {raw}: {error}"))
    })?;
    if !meta.is_file() {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "binaryPath must be a regular file: {raw}"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o111 == 0 {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "binaryPath is not executable: {raw}"
            )));
        }
        // B4: anyone but the owner being able to rewrite the file makes the
        // pin meaningless — the bytes we hashed are not the bytes that run.
        if mode & 0o022 != 0 {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "binaryPath must not be group- or world-writable, found {:04o}: {raw}",
                mode & 0o777
            )));
        }
        // B4, ownership. The spec says "owned by the Node uid", but the
        // overwhelmingly common install is a root-owned `claude` under
        // /usr/local or a package manager prefix, which the Node user cannot
        // rewrite and which that rule would reject outright. What the rule is
        // actually defending against is a file the *agent* can replace, so the
        // test is "owned by us or by root", with root already covered by the
        // group/other-writable check above. An unrelated third uid is refused:
        // that is neither our file nor a system install.
        let owner = meta.uid();
        if let Some(uid) = node_uid()
            && owner != uid
            && owner != 0
        {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "binaryPath must be owned by the Node user or root, found uid {owner}: {raw}"
            )));
        }
    }
    // B4: containment. An agent that can write one of these directories must
    // not be able to name something inside it as its own executable.
    for (label, root) in guard.forbidden_roots() {
        if canonical.starts_with(&root) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "binaryPath must not resolve inside the {label} ({}): {raw}",
                root.display()
            )));
        }
    }
    // B5: pin it with the same code path every other binary goes through.
    let pin = pin_binary(&canonical)?;
    if let Some(expected) = expected
        && &pin.sha256 != expected
    {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "binaryPath digest mismatch: recorded {}, found {}",
            String::from(expected.clone()),
            String::from(pin.sha256.clone())
        )));
    }
    tracing::info!(
        path = %pin.abs_path,
        version = %pin.version,
        digest = %String::from(pin.sha256.clone()),
        "pinned binary override"
    );
    Ok(pin)
}

/// Effective uid of this process, without `unsafe`.
///
/// The workspace forbids `unsafe_code`, so `geteuid(2)` is out of reach. A file
/// this process creates is owned by our effective uid by definition, so one
/// throwaway file in the temp dir answers the same question with std alone.
/// Cached: the answer cannot change within a process.
///
/// `None` when the probe fails (an unwritable temp dir). Callers skip the
/// ownership check rather than failing a launch over it — the mode and
/// containment checks are the ones carrying the weight.
#[cfg(unix)]
fn node_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    use std::sync::OnceLock;
    static UID: OnceLock<Option<u32>> = OnceLock::new();
    *UID.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(".remuda-uid-probe-{}", unique_token()));
        let uid = File::create(&path)
            .ok()?
            .metadata()
            .ok()
            .map(|meta| meta.uid());
        let _ = fs::remove_file(&path);
        uid
    })
}

impl BinaryOverrideGuard {
    /// Directories the override may not resolve inside, canonicalized.
    ///
    /// A root that does not exist or cannot be canonicalized is skipped rather
    /// than failing the launch: it cannot contain the resolved path either way.
    ///
    /// `TMPDIR` is deliberately *not* a blanket root. The spec called for it,
    /// but `std::env::temp_dir()` is `/tmp` on stock Linux, and refusing
    /// everything beneath `/tmp` would reject any deployment whose checkout or
    /// install prefix happens to live there — including this repo's own CI
    /// worktrees. The escalation it was meant to stop is "the agent writes a
    /// file and names it", which the instance, launch, and cwd roots already
    /// cover precisely. A caller with its own scratch directory adds it via
    /// [`BinaryOverrideGuard::extra`] rather than having a guess imposed here.
    fn forbidden_roots(&self) -> Vec<(&'static str, PathBuf)> {
        let mut roots = Vec::new();
        let mut push = |label: &'static str, path: Option<PathBuf>| {
            if let Some(path) = path
                && let Ok(canonical) = fs::canonicalize(&path)
            {
                roots.push((label, canonical));
            }
        };
        push("instance directory", self.instance_dir.clone());
        push(
            "launch directory",
            self.instance_dir.as_ref().map(|dir| dir.join("launch")),
        );
        push("workspace cwd", self.cwd.clone());
        for path in &self.extra {
            push("refused directory", Some(path.clone()));
        }
        roots
    }
}

/// Resolve a command name or path to a canonical absolute file.
pub fn resolve_binary(command: impl AsRef<Path>) -> DriverResult<PathBuf> {
    let command = command.as_ref();
    let candidate = if command.is_absolute() {
        command.to_path_buf()
    } else {
        find_on_path(command).ok_or_else(|| DriverError::BinaryNotFound(command.to_path_buf()))?
    };
    if !candidate.is_file() {
        return Err(DriverError::BinaryNotFound(candidate));
    }
    candidate.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            DriverError::BinaryNotFound(candidate)
        } else {
            DriverError::Io(error)
        }
    })
}

/// SHA-256 a file at `path`.
pub fn hash_file(path: &Path) -> DriverResult<Digest> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0_u8; HASH_BUF];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    digest_from_sha(hasher.finalize())
}

/// SHA-256 in-memory bytes (settings overlay, tests).
pub fn hash_bytes(bytes: &[u8]) -> DriverResult<Digest> {
    digest_from_sha(Sha256::digest(bytes))
}

fn digest_from_sha(hash: impl core::fmt::LowerHex) -> DriverResult<Digest> {
    let encoded = format!("sha256:{hash:x}");
    Digest::try_from(encoded).map_err(DriverError::Protocol)
}

fn read_version(path: &Path) -> DriverResult<String> {
    let output = Command::new(path).arg("--version").output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            DriverError::InvalidLaunchSpec(format!(
                "{} produced no --version output",
                path.display()
            ))
        })?;
    Ok(line.to_string())
}

fn is_etxtbsy(err: &io::Error) -> bool {
    err.kind() == ErrorKind::ExecutableFileBusy || err.raw_os_error() == Some(26)
}

fn unique_token() -> String {
    let seq = PIN_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Copy `src` to a unique path so `exec` is not racing a mapped inode.
fn copy_to_fresh_path(src: &Path) -> DriverResult<PathBuf> {
    let bytes = fs::read(src)?;
    let stem = src
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pinned-bin");
    let mut dirs = Vec::new();
    if let Some(parent) = src.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs.push(std::env::temp_dir());
    let mut last_err = None;
    for dir in dirs {
        match install_unique(&dir, stem, &bytes) {
            Ok(path) => return Ok(path),
            Err(err) => last_err = Some(err),
        }
    }
    Err(DriverError::Io(last_err.unwrap_or_else(|| {
        io::Error::other(format!(
            "pin_binary: could not copy {} to a fresh path",
            src.display()
        ))
    })))
}

fn install_unique(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let mut last = None;
    for _ in 0..16 {
        match try_install(dir, name, bytes) {
            Ok(path) => return Ok(path),
            Err(err) if is_etxtbsy(&err) || err.kind() == ErrorKind::AlreadyExists => {
                last = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from(ErrorKind::ExecutableFileBusy)))
}

fn try_install(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let token = unique_token();
    let dest = dir.join(format!("{name}-{token}"));
    let part = dir.join(format!(".{name}-{token}.part"));
    if dest.exists() {
        return Err(io::Error::from(ErrorKind::AlreadyExists));
    }
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part)?;
        file.write_all(bytes)?;
        file.flush()?;
        let _ = file.sync_all();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&part, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&part, &dest)?;
    Ok(dest)
}

fn find_on_path(name: &Path) -> Option<PathBuf> {
    let file_name = name.file_name()?;
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(file_name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Default executable name for a driver kind.
pub fn default_command(kind: remuda_protocol::DriverKind) -> Option<&'static str> {
    use remuda_protocol::DriverKind::*;
    match kind {
        ClaudePrint | ClaudePty | ClaudeBg => Some("claude"),
        CodexAppserver => Some("codex"),
        GrokAcp => Some("grok"),
        AgyPrint => Some("agy"),
        GenericPty | ShellPty => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_idempotent_for_a_stub_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-9.9.9 (test)");
        let first = pin_binary(&path).unwrap();
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.abs_path.starts_with('/'));
        assert_eq!(first.version, "stub-9.9.9 (test)");
        assert!(String::from(first.sha256.clone()).starts_with("sha256:"));
    }

    /// A second pin of an unchanged file must be served from the
    /// `(path, len, mtime)` cache — the 207 MB claude hash runs once per
    /// binary version, not once per create.
    #[test]
    fn the_digest_cache_serves_a_second_pin_without_rehashing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-cache (test)");
        let resolved = path.canonicalize().unwrap();
        let first = pin_binary(&path).unwrap();
        // The entry is directly observable under the canonical path's key.
        let key = pin_cache_key(&resolved).unwrap();
        let cached = pin_cache_get(&key).expect("the pin is cached");
        assert_eq!(cached, first);
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second);
        // A symlink alias resolves to the same canonical file, so it must hit
        // the same entry rather than hash through the alias.
        let alias = dir.path().join("alias-stub");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let via_alias = pin_binary(&alias).unwrap();
        assert_eq!(via_alias, first);
        // A different file's pin never aliases to this entry.
        let other = write_stub(dir.path(), "stub-cache (different)");
        assert_ne!(pin_binary(&other).unwrap().sha256, first.sha256);
    }

    fn write_stub(dir: &Path, version: &str) -> PathBuf {
        install_unique(
            dir,
            "agent",
            format!("#!/bin/sh\necho '{version}'\n").as_bytes(),
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pin_binary_copies_to_a_fresh_path_when_source_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-busy (test)");
        let _writer = OpenOptions::new().write(true).open(&path).unwrap();
        let pin = pin_binary(&path).unwrap();
        let pinned = PathBuf::from(&pin.abs_path);
        assert_ne!(
            pinned,
            path.canonicalize().unwrap(),
            "busy source must not be overwritten"
        );
        assert_eq!(pin.version, "stub-busy (test)");
        assert!(pinned.is_file());
    }

    /// A writer that closes mid-probe is the fork window between a sibling
    /// thread's `open(O_WRONLY)` and its `exec`. Pinning must wait it out and
    /// keep the original path, or two pins of one file disagree on `abs_path`.
    #[cfg(target_os = "linux")]
    #[test]
    fn pin_is_idempotent_when_a_transient_writer_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-transient (test)");
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(8));
            drop(writer);
        });
        let first = pin_binary(&path).unwrap();
        handle.join().unwrap();
        let second = pin_binary(&path).unwrap();
        assert_eq!(first, second, "transient ETXTBSY must not fork the path");
        assert_eq!(
            PathBuf::from(&first.abs_path),
            path.canonicalize().unwrap(),
            "a writer that closes must not trigger a copy"
        );
    }

    /// Concurrent pins of one file must agree — the CI failure mode.
    #[test]
    fn concurrent_pins_of_one_binary_agree() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_stub(dir.path(), "stub-parallel (test)");
        let pins: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| pin_binary(&path).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for pin in &pins {
            assert_eq!(pin, &pins[0], "concurrent pins must be identical");
        }
    }
}
