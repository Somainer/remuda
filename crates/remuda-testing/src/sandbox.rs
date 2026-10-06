//! Temp-home sandbox shared by the test doubles (c-resumehome round 3).
//!
//! # Threat model
//!
//! A fake must never create or modify files in the operator's real home.
//! Round 2 authorized anything under `$TMPDIR`, which is wrong:
//!
//! - an inherited `TMPDIR=$HOME` authorized the operator's whole home;
//! - a home whose `projects` entry was a symlink into the real
//!   `~/.claude/projects` passed the path checks;
//! - negative tests skipped themselves whenever the cargo scratch dir
//!   happened to live under `/tmp`.
//!
//! # Model
//!
//! Writes are authorized only inside a **deliberately allocated root** marked
//! with the [`ROOT_SENTINEL`] file. Roots are created with
//! [`TempHome::allocate`] (tests) or [`private_temp_home`] (a fake that was
//! spawned with no configured home). Authorization is verified by an
//! `O_NOFOLLOW` directory walk — never by `$TMPDIR`/`$HOME` ancestry — and the
//! actual writes go through the `remuda-fdsafe` descriptor walk, so a symlink
//! inside an allocated root (e.g. `projects -> ~/.claude/projects`) cannot
//! redirect a single byte.
//!
//! A deliberate manual run of a fake outside a test harness can opt out with
//! the binary-specific `*_ALLOW_HOME_WRITE=1` knob; no test sets it.

use remuda_fdsafe::{DirFd, FdErrorKind, LeafKind};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// File marking a directory as a deliberately allocated fake write root.
pub const ROOT_SENTINEL: &str = ".remuda-fake-root";
/// Env var overriding the base under which [`TempHome::allocate`] creates
/// roots (must already exist; created through an `O_NOFOLLOW` walk).
pub const HOME_BASE_ENV: &str = "REMUDA_FAKE_HOME_BASE";
/// How many levels the sentinel search climbs from a configured home.
const SENTINEL_DEPTH: usize = 10;
static ALLOC_SEQ: AtomicU64 = AtomicU64::new(0);

/// Name of the escape-hatch env var that authorizes writes outside an
/// allocated root for a deliberate manual run.
pub fn outside_writes_allowed(allow_env: &str) -> bool {
    std::env::var_os(allow_env).as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// Fixed canonical system temp bases. `std::env::temp_dir()` honors
/// `$TMPDIR` and can equal `$HOME`, so it is deliberately NOT used as a base.
fn system_temp_bases() -> Vec<PathBuf> {
    let mut bases = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    if let Ok(path) = std::env::current_exe() {
        // macOS keeps per-user system temp under /private/var/folders; on
        // Linux /tmp is canonical. Nothing here ever trusts $TMPDIR.
        if let Some(parent) = path
            .parent()
            .map(Path::to_path_buf)
            .filter(|p| p.starts_with("/private/var/folders"))
        {
            bases.push(parent);
        }
    }
    bases
}

/// Lexically normalize `path` (`.`/`..` resolved, no symlink following),
/// making it absolute.
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A deliberately allocated write root. Creating the guard makes the private
/// directory (0700) plus its sentinel; dropping it removes the whole tree
/// through descriptor-relative unlinks, so a symlink inside it can never drag
/// the cleanup elsewhere.
#[derive(Debug)]
pub struct TempHome {
    root: PathBuf,
}

impl TempHome {
    /// Allocate a fresh, RANDOMIZED private root directly under a pinned base
    /// (a fixed system temp, or [`HOME_BASE_ENV`]).
    ///
    /// Round 4 item 8: the directory is created with an exclusive `mkdirat`
    /// (an existing name fails — it never adopts or removes a directory it did
    /// not create), 0700, and the OPENED fd is verified to be owned by this
    /// process and not group/other-writable before the sentinel is written.
    /// A few random names are tried; every directory created by a failed
    /// attempt is removed by this function only.
    pub fn allocate(label: &str) -> std::io::Result<Self> {
        let base = match std::env::var_os(HOME_BASE_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => system_temp_bases()
                .into_iter()
                .find(|base| base.is_dir())
                .ok_or_else(|| {
                    std::io::Error::other("no system temp base available for a fake home")
                })?,
        };
        let base_fd = DirFd::anchor_existing(&base)?;
        for _ in 0..8 {
            let name = format!(
                "remuda-fake-{}-{}-{}-{}",
                std::process::id(),
                ALLOC_SEQ.fetch_add(1, Ordering::Relaxed),
                label,
                simple_random_suffix()
            );
            let name_bytes = name.as_bytes();
            let made = match base_fd.create_subdir_excl(name_bytes) {
                Ok(dir) => dir,
                // Randomized name collided: try another.
                Err(error) if error.is_already_exists() => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            // Verify ownership and mode on the fd we just created.
            if !dir_is_ours_and_private(&made)? {
                let _ = base_fd.remove_private_tree(name_bytes);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "allocated fake root is not a private 0700 directory owned by this process",
                ));
            }
            let sentinel_result =
                made.create_leaf_excl(ROOT_SENTINEL.as_bytes())
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(b"remuda fake write root\n").map_err(|error| {
                            remuda_fdsafe::FdError::new(
                                "sentinel",
                                FdErrorKind::Other(format!("sentinel write: {error}")),
                            )
                        })?;
                        Ok(())
                    });
            if let Err(error) = sentinel_result {
                let _ = base_fd.remove_private_tree(name_bytes);
                return Err(error.into());
            }
            let root = normalize(&base.join(name));
            return Ok(Self { root });
        }
        Err(std::io::Error::other(
            "could not allocate a unique fake root after several attempts",
        ))
    }

    /// The allocated root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// `root/<rel>` for building test layouts.
    pub fn child(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Mark an EXISTING test-owned directory as an allocated root by writing
    /// the sentinel into it. Used by spawn helpers that already built a
    /// `tempfile` directory with their own layout; after this, every subtree
    /// (the fake homes they pass explicitly) is authorized. The directory is
    /// not deleted on drop (the caller owns its lifetime).
    pub fn adopt(dir: &Path) -> std::io::Result<()> {
        mark_root(dir)
    }
}

/// A short non-cryptographic random suffix (PID+counter+time already make the
/// allocation name unique; the suffix just avoids racing another process that
/// guessed the same counter).
fn simple_random_suffix() -> u32 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ (std::process::id().wrapping_mul(2_654_435_761))
}

/// The opened directory fd must be owned by this euid and carry no
/// group/other access. Used for a directory THIS crate exclusively allocated.
fn dir_is_ours_and_private(dir: &DirFd) -> std::io::Result<bool> {
    let euid = nix::unistd::geteuid().as_raw();
    if dir.uid()? != euid {
        return Ok(false);
    }
    Ok(dir.mode()? & 0o077 == 0)
}

/// The opened directory fd must be owned by this euid and not be world-writable
/// (group access is acceptable for an adopted test `tempfile` dir). Used to
/// accept a pre-existing test-owned directory as a root.
fn dir_is_ours_and_not_world_writable(dir: &DirFd) -> std::io::Result<bool> {
    let euid = nix::unistd::geteuid().as_raw();
    if dir.uid()? != euid {
        return Ok(false);
    }
    Ok(dir.mode()? & 0o002 == 0)
}

impl Drop for TempHome {
    fn drop(&mut self) {
        // Remove exactly the randomized subtree this allocation created; its
        // parent is anchored (system mount symlinks resolved once).
        if let Some(parent) = self.root.parent()
            && let Some(name) = self.root.file_name()
            && let Ok(dir) = DirFd::anchor_existing(parent)
        {
            let _ = dir.remove_private_tree(name.as_encoded_bytes());
        }
    }
}

/// Create a private per-process home under a FIXED system temp (never
/// `$TMPDIR`), marked as an allocated root. A fake uses this when it was
/// spawned with no configured home, so it persists nowhere near the
/// operator's real home even when `TMPDIR=$HOME`.
#[must_use]
pub fn private_temp_home(label: &str) -> PathBuf {
    let name = format!("{label}-{}", std::process::id());
    for base in system_temp_bases() {
        let candidate = normalize(&base.join(&name));
        // Anchor the fixed mount, then ensure the per-process subtree and
        // verify it is ours and private before accepting any sentinel.
        let Ok(base_fd) = DirFd::anchor_existing(&base) else {
            continue;
        };
        let Ok(dir) = base_fd.ensure_subdir(name.as_bytes()) else {
            continue;
        };
        // The private per-process home created by the FAKE itself is private;
        // this path also tolerates an adopted test dir with group access.
        if !dir_is_ours_and_not_world_writable(&dir).unwrap_or(false) {
            continue;
        }
        if dir.create_leaf_excl(ROOT_SENTINEL.as_bytes()).is_ok()
            || matches!(dir.classify_leaf(ROOT_SENTINEL.as_bytes()), Ok(Some(entry)) if entry.kind == LeafKind::Regular)
        {
            return candidate;
        }
    }
    // Last resort: do not persist anywhere.
    candidate_unusable()
}

fn candidate_unusable() -> PathBuf {
    PathBuf::from("/dev/null/.no-fake-home")
}

/// Find the allocated root governing `path` by climbing its REAL ancestor
/// directories (each realpath'd once by the anchor walk) up toward `/` until
/// a regular [`ROOT_SENTINEL`] is found. A sentinel is accepted ONLY when the
/// directory holding it is owned by this process and is not world-writable,
/// and is not itself a shared temp mount — a planted
/// `/tmp/.remuda-fake-root` therefore never authorizes all of `/tmp`. Group
/// access on an adopted test `tempfile` dir is tolerated (group-writable
/// never is).
fn find_allowed_root(path: &Path) -> Option<PathBuf> {
    let absolute = normalize(path);
    let mut ancestor: Option<PathBuf> = Some(absolute);
    let mut tried = 0;
    while let Some(dir_path) = ancestor {
        tried += 1;
        if tried > SENTINEL_DEPTH + 4 {
            return None;
        }
        if let Ok(dir) = DirFd::anchor_existing(&dir_path)
            && matches!(dir.classify_leaf(ROOT_SENTINEL.as_bytes()), Ok(Some(entry)) if entry.kind == LeafKind::Regular)
            && sentinel_dir_is_trustworthy(&dir, &dir_path)
        {
            return Some(dir_path);
        }
        ancestor = dir_path
            .parent()
            .filter(|parent| parent != &dir_path)
            .map(Path::to_path_buf);
    }
    None
}

/// A sentinel marks a real root only when its directory is owned by this
/// process, private (no group/other access), and not a shared kernel temp
/// mount itself.
fn sentinel_dir_is_trustworthy(dir: &DirFd, path: &Path) -> bool {
    if !dir_is_ours_and_not_world_writable(dir).unwrap_or(false) {
        return false;
    }
    let normalized = normalize(path);
    !system_temp_bases()
        .iter()
        .any(|base| normalize(base) == normalized)
}

/// Whether `path` (lexically) lies below a FIXED kernel temp mount. Unlike
/// round 2 this deliberately does NOT consult `$TMPDIR`: an inherited
/// `TMPDIR=$HOME` must not authorize the operator's real home.
fn under_fixed_temp(path: &Path) -> bool {
    let path = normalize(path);
    system_temp_bases()
        .iter()
        .any(|base| path.starts_with(base))
}

/// Mark `dir` itself as an allocated root (fd walk + sentinel). Idempotent:
/// an existing regular sentinel is accepted. A symlink at any component is
/// refused. The shared mount points themselves (`/tmp`, `/var/tmp`) and any
/// world-writable directory are never marked: adoption authorizes one
/// dedicated test directory, never every descendant of the temp mount.
fn mark_root(dir: &Path) -> std::io::Result<()> {
    let normalized_dir = normalize(dir);
    if system_temp_bases()
        .iter()
        .any(|base| normalize(base) == normalized_dir)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to mark a shared temp mount as a fake root",
        ));
    }
    // Anchor the existing directory (system mount symlinks resolved once).
    let dir_fd = DirFd::anchor_or_create(&normalized_dir)?;
    // Round 4 item 8: verify the OPENED directory is owned by us and private;
    // never a path re-stat, and never trust a world/group-writable or
    // foreign-owned directory just because it carries a sentinel file.
    if !dir_is_ours_and_not_world_writable(&dir_fd)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to mark a world-writable or foreign-owned directory as a fake root",
        ));
    }
    match dir_fd.classify_leaf(ROOT_SENTINEL.as_bytes())? {
        Some(entry) if entry.kind == LeafKind::Regular => Ok(()),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "a non-regular entry already occupies the fake-root sentinel name",
        )),
        None => {
            let mut sentinel = dir_fd.create_leaf_excl(ROOT_SENTINEL.as_bytes())?;
            sentinel.write_all(b"remuda fake write root\n")?;
            Ok(())
        }
    }
}

/// Authorize an explicit directory, adopting it when it lives under a fixed
/// temp mount.
///
/// - already inside an allocated root: accepted;
/// - under `/tmp`/`/var/tmp` (never `$TMPDIR`, so `TMPDIR=$HOME` does not
///   authorize the operator home): the directory is marked with the sentinel
///   through the `O_NOFOLLOW` walk and accepted;
/// - the escape knob set: accepted verbatim (deliberate manual run only);
/// - anything else: refused with a named [`PermissionDenied`].
pub fn adopt_or_refuse(dir: &Path, allow_env: &str) -> std::io::Result<()> {
    if outside_writes_allowed(allow_env) || find_allowed_root(dir).is_some() {
        return Ok(());
    }
    if under_fixed_temp(dir) && mark_root(dir).is_ok() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "test fake refuses to write {}: it is not an allocated fake root and is not under a \
             fixed system temp mount (set {allow_env}=1 only for a deliberate manual run)",
            normalize(dir).display()
        ),
    ))
}

/// Like [`adopt_or_refuse`] for a FILE target: its parent directory is the
/// thing adopted/authorized.
pub fn adopt_or_refuse_file(path: &Path, allow_env: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    adopt_or_refuse(parent, allow_env)
}

/// Reject an explicit write target that is not inside a sentinel-marked
/// allocated root (the binary-specific escape knob excepted).
/// Authorize an explicit HOME directory: sentinel root subtree or the
/// deliberate-run knob. Unlike [`ensure_path_in_temp`] this never adopts a
/// fixed-temp directory — the spawn helper must have allocated it.
pub fn ensure_home_allocated(path: &Path, allow_env: &str) -> std::io::Result<()> {
    if outside_writes_allowed(allow_env) || find_allowed_root(path).is_some() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "test fake refuses an explicit home outside an allocated root: {} (allocate it \
                 with remuda_testing::sandbox::TempHome, or set {allow_env}=1 for a deliberate \
                 manual run)",
                normalize(path).display()
            ),
        ))
    }
}

/// Reject a FILE write target whose parent directory is not an allocated root
/// subtree, unless it is under a fixed system temp mount (adopted) or the
/// escape knob is set.
pub fn ensure_path_in_temp(path: &Path, allow_env: &str) -> std::io::Result<()> {
    adopt_or_refuse_file(path, allow_env)
}

/// Reject a DIRECTORY write target: the directory itself (not its parent) is
/// adopted/marked when it lives under a fixed system temp. Use this for
/// config/transcript HOME directories, where adopting the parent (often `/tmp`
/// itself) would be wrong.
pub fn ensure_dir_in_temp(dir: &Path, allow_env: &str) -> std::io::Result<()> {
    adopt_or_refuse(dir, allow_env)
}

/// Open an EXISTING regular file for append via the fd-relative walk: anchor
/// its parent (realpath once), reject a symlink/FIFO/device leaf, and return
/// an `O_APPEND|O_NOFOLLOW|O_NONBLOCK` fd. Every fake-harness resume append
/// must go through this so a symlinked `H/projects/…` cannot redirect a
/// write outside the allocated home.
pub fn open_append_file(path: &Path, allow_env: &str) -> std::io::Result<std::fs::File> {
    let absolute = normalize(path);
    let parent = absolute
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "write target has no parent directory: {}",
                    absolute.display()
                ),
            )
        })?;
    // Authorization governs the DIRECTORY the file lives in.
    adopt_or_refuse(parent, allow_env)?;
    let Some(name) = absolute.file_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("write target has no file name: {}", absolute.display()),
        ));
    };
    let dir_fd = DirFd::anchor_existing(parent)?;
    dir_fd
        .open_append_leaf(name.as_encoded_bytes())
        .map_err(std::io::Error::from)
}

/// See [`ensure_path_in_temp`] — current name kept for callers that think of
/// the check as "inside an allocated root" rather than "in the temp tree".
pub fn ensure_allowed(path: &Path, allow_env: &str) -> std::io::Result<()> {
    if outside_writes_allowed(allow_env) || find_allowed_root(path).is_some() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "test fake refuses to write {}: it is not inside a TempHome::allocate root \
                 (set {allow_env}=1 only for a deliberate manual run)",
                normalize(path).display()
            ),
        ))
    }
}

/// Resolve a sandboxed home for the `(explicit_env, home_env, fallback)`
/// triple used by every fake.
///
/// - an explicit `<explicit_env>` inside an allocated root wins; outside one
///   is a loud error;
/// - an implicit `$<home_env>/<fallback>` is used only inside an allocated
///   root; an operator home outside one yields `None` (the caller skips
///   persistence or mints [`private_temp_home`]);
/// - with the escape knob, both are honored verbatim.
pub fn sandboxed_home(
    explicit_env: &str,
    home_env: &str,
    fallback: &str,
    allow_env: &str,
) -> std::io::Result<Option<PathBuf>> {
    if outside_writes_allowed(allow_env) {
        if let Some(path) = std::env::var_os(explicit_env)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return Ok(Some(path));
        }
        if let Some(home) = std::env::var_os(home_env).filter(|value| !value.is_empty()) {
            return Ok(Some(PathBuf::from(home).join(fallback)));
        }
        return Ok(None);
    }
    if let Some(configured) = std::env::var_os(explicit_env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        // An explicitly named home is accepted ONLY when it is an allocated
        // root subtree (or the deliberate-run knob is set). Fixed-temp
        // ancestry alone is not enough here: item 9 requires the spawn helper
        // to have deliberately allocated that directory.
        if outside_writes_allowed(allow_env) || find_allowed_root(&configured).is_some() {
            return Ok(Some(configured));
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "test fake refuses an explicit {explicit_env} outside an allocated root: {} \
                 (allocate it with remuda_testing::sandbox::TempHome, or set {allow_env}=1 for \
                 a deliberate manual run)",
                normalize(&configured).display()
            ),
        ));
    }
    // An implicit `$HOME/<fallback>` is used ONLY when it already sits inside
    // an allocated root. A bare temp-mounted `$HOME` does not self-adopt: the
    // spawn helper must have allocated it deliberately, and an operator home
    // (allocated root or not) never matches.
    let implicit = std::env::var_os(home_env)
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(fallback))
        .filter(|home| find_allowed_root(home).is_some());
    Ok(implicit)
}

/// Open (create if absent) and append to a regular transcript file at
/// `<home>/projects/<encoded-cwd>/<file_name>`, entirely through the fd walk.
/// The home must already be authorized by the caller ([`ensure_allowed`]).
pub fn append_project_transcript(
    home: &Path,
    project_slug: &str,
    file_name: &str,
) -> std::io::Result<std::fs::File> {
    let home_fd = DirFd::anchor_or_create(&normalize(home))?;
    append_project_transcript_fd(&home_fd, project_slug, file_name)
}

/// fd-relative form: `home_fd` is already pinned with the correct anchor
/// semantics, so `projects/<slug>` is always walked link-free below it.
pub fn append_project_transcript_fd(
    home_fd: &DirFd,
    project_slug: &str,
    file_name: &str,
) -> std::io::Result<std::fs::File> {
    let projects = home_fd.ensure_subdir(b"projects")?;
    let slug = projects.ensure_subdir(project_slug.as_bytes())?;
    let name = file_name.as_bytes();
    match slug.classify_leaf(name)? {
        Some(entry) if entry.kind == LeafKind::Regular => Ok(slug.open_append_leaf(name)?),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("transcript {file_name} is a symlink or non-regular file"),
        )),
        None => Ok(slug.create_leaf_excl(name)?),
    }
}

/// Open an EXISTING regular transcript for appending; `NotFound` if absent or
/// not a regular file (resume must never silently create).
pub fn open_existing_transcript_append(
    home: &Path,
    project_slug: &str,
    file_name: &str,
) -> std::io::Result<std::fs::File> {
    let home_fd = DirFd::anchor_existing(&normalize(home))?;
    let projects = home_fd.subdir(b"projects")?;
    let slug = projects.subdir(project_slug.as_bytes())?;
    slug.open_append_leaf(file_name.as_bytes())
        .map_err(std::io::Error::from)
}

/// Pin the Claude config home with the correct trust semantics.
///
/// - An EXPLICIT `<explicit_env>` path is the configured home: it is the
///   trusted anchor, realpath'd once (crossing system mount symlinks).
/// - Otherwise an IMPLICIT `$<home_env>/<fallback>` is resolved by anchoring
///   `$<home_env>` and WALKING `<fallback>` with `O_NOFOLLOW`; a symlinked
///   fallback directory (e.g. a planted `~/.claude`) inside an allocated home
///   is refused rather than canonicalized through.
/// - With neither, the private per-process home is minted and the fallback
///   walked link-free.
pub fn claude_config_home_fd(
    explicit_env: &str,
    home_env: &str,
    fallback: &str,
    allow_env: &str,
) -> std::io::Result<Option<DirFd>> {
    if let Some(explicit) = std::env::var_os(explicit_env).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(explicit);
        ensure_home_allocated(&path, allow_env)?;
        return Ok(Some(DirFd::anchor_or_create(&path)?));
    }
    if let Some(home) = std::env::var_os(home_env).filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        // An implicit `$HOME/<fallback>` that is not under an allocated root:
        // silently skip persistence (None) — the turn still runs, but no
        // `~/.claude` tree is created. The EXPLICIT env case above fails loud.
        if find_allowed_root(&home.join(fallback)).is_none() {
            return Ok(None);
        }
        let home_fd = DirFd::anchor_or_create(&home)?;
        return Ok(Some(home_fd.ensure_subdir(fallback.as_bytes())?));
    }
    Ok(None)
}

/// Mint the private fallback home (used when no explicit/implicit home
/// exists) and pin it with the fallback subdirectory walked link-free.
pub fn private_claude_home_fd(label: &str, fallback: &str) -> std::io::Result<DirFd> {
    let home = private_temp_home(label);
    // No usable fixed system temp base: private_temp_home returns a sentinel
    // path that is intentionally unwritable.
    if !home.is_absolute() || home.parent().is_none() || !under_fixed_temp(&home) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no fixed system temp base available for a private fake home",
        ));
    }
    let home_fd = DirFd::anchor_or_create(&home)?;
    Ok(home_fd.ensure_subdir(fallback.as_bytes())?)
}

/// Write `body` to `path` atomically (0600 temp + rename) through the fd walk.
/// Parent directories are created `O_NOFOLLOW`; `path` must be inside an
/// allocated root.
pub fn write_allowed_file(path: &Path, body: &[u8], allow_env: &str) -> std::io::Result<()> {
    let absolute = normalize(path);
    ensure_allowed(&absolute, allow_env)?;
    let parent = absolute
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("write target has no parent directory"))?;
    let Some(name) = absolute.file_name() else {
        return Err(std::io::Error::other("write target has no file name"));
    };
    let parent_fd = DirFd::anchor_or_create(parent)?;
    // Refuse a symlink or special file at the destination up front; the final
    // rename also never replaces a directory or link.
    if let Some(entry) = parent_fd.classify_leaf(name.as_encoded_bytes())?
        && entry.kind != LeafKind::Regular
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("write target {} is not a regular file", absolute.display()),
        ));
    }
    let tmp_name = format!(
        ".write-tmp-{}-{}",
        std::process::id(),
        ALLOC_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let outcome: std::io::Result<()> = (|| {
        let mut file = parent_fd.create_leaf_excl(tmp_name.as_bytes())?;
        file.write_all(body)?;
        file.sync_all()?;
        parent_fd.rename(tmp_name.as_bytes(), &parent_fd, name.as_encoded_bytes())?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = parent_fd.unlink_file(tmp_name.as_bytes());
    }
    outcome
}

/// Whether `path` currently sits inside an allocated root (fd-walk verdict).
#[must_use]
pub fn is_in_allocated_root(path: &Path) -> bool {
    find_allowed_root(path).is_some()
}

/// Open an absolute `path` for appending, creating parent directories and the
/// leaf (0600) entirely through the descriptor walk. `path` must be inside an
/// allocated root; a symlink at any level, including the leaf, is refused.
pub fn append_or_create(path: &Path, allow_env: &str) -> std::io::Result<std::fs::File> {
    let absolute = normalize(path);
    ensure_allowed(&absolute, allow_env)?;
    let parent = absolute
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("write target has no parent directory"))?;
    let Some(name) = absolute.file_name() else {
        return Err(std::io::Error::other("write target has no file name"));
    };
    let parent_fd = DirFd::anchor_or_create(parent)?;
    match parent_fd.classify_leaf(name.as_encoded_bytes())? {
        Some(entry) if entry.kind == LeafKind::Regular => {
            Ok(parent_fd.open_append_leaf(name.as_encoded_bytes())?)
        }
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "test fake refuses to write through a symlink/non-regular file: {}",
                absolute.display()
            ),
        )),
        None => Ok(parent_fd.create_leaf_excl(name.as_encoded_bytes())?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allocated_root_authorizes_only_its_own_subtree() {
        let root = TempHome::allocate("unit").expect("allocate");
        let inside = root.child("projects/slug/x.jsonl");
        assert!(
            is_in_allocated_root(&inside),
            "the root governs its subtree"
        );
        // Escaping one root must not reach another allocated root: a sibling
        // prefix is not membership.
        let sibling = root.path().parent().unwrap().join(format!(
            "{}-sibling",
            root.path().file_name().unwrap().to_string_lossy()
        ));
        assert!(
            !is_in_allocated_root(&sibling),
            "a sibling is outside the root"
        );
        assert!(ensure_allowed(&sibling, "FAKE_TEST_NO_SUCH_KNOB_X").is_err());
    }

    #[test]
    fn tmpdir_or_home_ancestry_alone_never_authorizes() {
        // Whatever $TMPDIR/$HOME point at, a random non-marked directory is not
        // an allocated root.
        let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sandbox-scratch-unit");
        assert!(!is_in_allocated_root(&scratch));
        assert!(ensure_allowed(&scratch, "FAKE_TEST_NO_SUCH_KNOB_Y").is_err());
    }

    #[test]
    fn writes_through_the_walk_refuse_a_symlinked_projects_dir() {
        use std::os::unix::fs::symlink;
        let root = TempHome::allocate("unit-symlink").expect("allocate");
        let home = root.child("home");
        std::fs::create_dir_all(&home).expect("home");
        let target = root.child("real-projects");
        std::fs::create_dir_all(&target).expect("target");
        symlink(&target, home.join("projects")).expect("projects link");
        let error = append_project_transcript(&home, "slug", "s.jsonl")
            .expect_err("a symlinked projects dir cannot be created through");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
        assert!(
            !target.join("slug/s.jsonl").exists(),
            "no bytes are written through the link"
        );
    }

    #[test]
    fn dotdot_and_separators_are_component_checked() {
        let root = TempHome::allocate("unit-components").expect("allocate");
        assert!(append_project_transcript(root.path(), "slug", "../escape.jsonl").is_err());
        assert!(append_project_transcript(root.path(), "slug/../../x", "y").is_err());
    }
}
