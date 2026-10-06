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
    /// Allocate a fresh root under [`HOME_BASE_ENV`] (if set) or the first
    /// writable fixed system temp.
    pub fn allocate(label: &str) -> std::io::Result<Self> {
        let seq = ALLOC_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("remuda-fake-{}-{seq}-{label}", std::process::id());
        let base = match std::env::var_os(HOME_BASE_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => system_temp_bases()
                .into_iter()
                .find(|base| base.is_dir())
                .ok_or_else(|| {
                    std::io::Error::other("no system temp base available for a fake home")
                })?,
        };
        let root = normalize(&base.join(name));
        // Create 0700 through the fd walk (a symlinked base component refuses).
        DirFd::open_or_create_abs(&root)
            .and_then(|dir| {
                let mut sentinel = dir.create_leaf_excl(ROOT_SENTINEL.as_bytes())?;
                sentinel
                    .write_all(b"remuda fake write root\n")
                    .map_err(|error| {
                        remuda_fdsafe::FdError::new(
                            "sentinel",
                            FdErrorKind::Other(error.to_string()),
                        )
                    })?;
                Ok(())
            })
            .map_err(std::io::Error::from)?;
        Ok(Self { root })
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

impl Drop for TempHome {
    fn drop(&mut self) {
        if let Some(parent) = self.root.parent()
            && let Some(name) = self.root.file_name()
            && let Ok(dir) = DirFd::open_existing_abs(parent)
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
        if DirFd::open_or_create_abs(&candidate).is_ok()
            && let Ok(dir) = DirFd::open_existing_abs(&candidate)
            && (dir.create_leaf_excl(ROOT_SENTINEL.as_bytes()).is_ok()
                || matches!(dir.classify_leaf(ROOT_SENTINEL.as_bytes()), Ok(Some(entry)) if entry.kind == LeafKind::Regular))
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

/// Find the allocated root governing `path` by walking its real ancestors
/// (`O_NOFOLLOW` at every component) upward until a regular
/// [`ROOT_SENTINEL`] is found. The file or its nearest parents need not exist
/// yet: the deepest existing ancestor is where the climb starts. `None`: no
/// allocated root governs the path.
fn find_allowed_root(path: &Path) -> Option<PathBuf> {
    let absolute = normalize(path);
    // Build the ancestor chain [path, parent, …, /] and try each from deepest
    // to shallowest; the first that opens is the deepest real ancestor.
    let mut ancestor = absolute.clone();
    let mut tried = 0;
    loop {
        tried += 1;
        if tried > SENTINEL_DEPTH + 4 {
            return None;
        }
        if let Ok(dir) = DirFd::open_existing_abs(&ancestor) {
            // From this real ancestor, climb the real directory tree with
            // `openat(AT_FDCWD..)`-independent `..` opens (via open_parent)
            // until a sentinel is seen.
            let mut current = dir;
            let mut current_path = ancestor.clone();
            for _ in 0..SENTINEL_DEPTH {
                if matches!(current.classify_leaf(ROOT_SENTINEL.as_bytes()), Ok(Some(entry)) if entry.kind == LeafKind::Regular)
                {
                    return Some(current_path);
                }
                current_path = current_path.parent().map(Path::to_path_buf)?;
                current = current.open_parent().ok()?;
            }
            return None;
        }
        ancestor = ancestor.parent().map(Path::to_path_buf)?;
    }
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
    let dir_fd = DirFd::open_or_create_abs(&normalized_dir)?;
    // Classify the OPENED directory, never a path re-stat: a swap cannot
    // downgrade the verdict.
    if dir_fd.is_world_writable().map_err(std::io::Error::from)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to mark a world-writable directory as a fake root",
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
    let home_fd = DirFd::open_or_create_abs(&normalize(home))?;
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
    let home_fd = DirFd::open_existing_abs(&normalize(home))?;
    let projects = home_fd.subdir(b"projects")?;
    let slug = projects.subdir(project_slug.as_bytes())?;
    slug.open_append_leaf(file_name.as_bytes())
        .map_err(std::io::Error::from)
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
    let parent_fd = DirFd::open_or_create_abs(parent)?;
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
    let parent_fd = DirFd::open_or_create_abs(parent)?;
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
