//! Descriptor-relative access to filesystem trees an adversary controls the
//! contents of (c-resumehome rounds 3–4).
//!
//! Every path reached through this module starts at a **trusted anchor** that
//! is realpath'd ONCE when pinned, and every component BELOW the anchor is
//! walked with `openat(…, O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC)`. System mount
//! symlinks ABOVE trusted roots are therefore harmless — on macOS `/tmp`,
//! `/var` and `/private/var/tmp` differ — while a symlink anywhere inside an
//! anchored tree always fails:
//!
//! - a symlinked `projects/` or `<session>/` directory can never redirect
//!   writes outside the pinned home;
//! - a symlinked leaf (`<S>.jsonl`, a sidecar, the transcript source) is
//!   never opened or copied through;
//! - leaves are classified with an exact `(mode & S_IFMT)` comparison: a
//!   socket (0140000) or other special file never matches `S_IFREG`;
//! - leaves are `fstatat`-classified before they are opened, and regular
//!   leaves (append included) open with `O_NONBLOCK` so a FIFO cannot block
//!   the opener; the opened descriptor is `fstat`-rechecked before use;
//! - every file a walk creates is `O_EXCL` relative to a pinned directory fd
//!   and every publish is a `renameat` between pinned fds, never a path
//!   string;
//! - copies read the OPENED descriptor and enforce a live byte cap, so a
//!   file that grows after enumeration cannot push a copy over budget.
//!
//! Names passed to single-component methods must be one opaque component (no
//! `/`, no NUL, no `.`/`..`). Multi-component walks additionally reject
//! `..`/`.` components explicitly; escaping an anchor is impossible.
//!
//! Self-contained: only `std` + `nix` ("fs","dir"), no Remuda knowledge.
//! The c-dirpicker directory picker builds the same Linux/macOS pattern
//! (macOS uses the same flags; no `O_PATH` is needed there). When that
//! lands, extract this crate into the shared home both use.

#![cfg(unix)]
// The crate owns the workspace's one audited openat(2) fd transfer
// (`File::from_raw_fd`); see Cargo.toml for why it does not inherit the
// workspace `forbid(unsafe_code)` lint.
#![allow(unsafe_code)]
#![deny(missing_docs)]

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag};
use nix::sys::stat::{FileStat, Mode};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};

/// What a directory entry is, classified without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafKind {
    /// `S_ISREG` exactly — the only kind [`DirFd::open_regular_leaf`] accepts.
    Regular,
    /// `S_ISLNK` — always rejected by this module.
    Symlink,
    /// `S_ISDIR`.
    Directory,
    /// FIFO, socket, char/block device, whiteout, anything else — never read
    /// or copied, skipped by callers and reported instead.
    Other,
}

/// One classified directory entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Single-component file name.
    pub name: Vec<u8>,
    /// Class from `fstatat(AT_SYMLINK_NOFOLLOW)` / the opened fd.
    pub kind: LeafKind,
    /// Size in bytes (0 for non-files).
    pub len: u64,
}

impl Entry {
    /// Exact regular-file test (`S_IFMT == S_IFREG`): a socket (0140000) is
    /// not a regular file.
    #[must_use]
    pub fn is_regular(&self) -> bool {
        self.kind == LeafKind::Regular
    }
}

/// A leaf opened `O_NOFOLLOW`, with the classification of the **opened fd**.
#[derive(Debug)]
pub struct OpenedLeaf {
    /// The opened descriptor (read-only).
    pub file: File,
    /// `fstat` on the opened fd — no path re-resolution.
    pub kind: LeafKind,
    /// Size from that same `fstat`.
    pub len: u64,
}

impl OpenedLeaf {
    /// The `(st_dev, st_ino, st_nlink)` triple of the opened file description.
    /// Two leaves are the same on-disk file iff dev+ino match — read from the
    /// opened fds, so a symlink swap after opening cannot change the verdict.
    /// `nlink` lets a caller reject a leaf that is still a hardlink to a file
    /// it did not itself write.
    pub fn identity(&self) -> Result<(u64, u64), FdError> {
        let stat = fstat_fd(self.file.as_raw_fd())?;
        Ok((dev_of(&stat), ino_of(&stat)))
    }

    /// Hard-link count on the opened fd (`st_nlink`).
    pub fn nlink(&self) -> Result<u64, FdError> {
        let stat = fstat_fd(self.file.as_raw_fd())?;
        #[allow(clippy::unnecessary_cast)]
        Ok(stat.st_nlink as u64)
    }
}

/// An owned directory fd pinned to one trusted directory.
#[derive(Debug)]
pub struct DirFd {
    file: File,
}

impl DirFd {
    /// Duplicate the directory descriptor (an independent `dup` of the same
    /// open directory description), so a caller can walk a chain without
    /// borrowing `self` mutably.
    pub fn self_clone(&self) -> DirFd {
        DirFd {
            file: self.file.try_clone().expect("dup directory fd"),
        }
    }
}

impl AsRawFd for DirFd {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// RAII exclusive advisory lock (`flock(2)`) held on a pinned directory fd.
///
/// `flock` locks are tied to the open file description: two independent
/// `openat` pins of the SAME directory (in two threads or two processes)
/// contend, while a [`DirFd::self_clone`] shares one lock. The held nix
/// [`Flock`] releases the lock on drop; a process that dies holding it
/// releases the kernel lock automatically, which is exactly what lets a later
/// attempt prove an earlier one is gone before removing its private temp tree.
#[must_use]
pub struct DirLock<'a> {
    // Exists for its Drop (LOCK_UN); never read.
    #[allow(dead_code)]
    held: Flock<File>,
    _pin: std::marker::PhantomData<&'a DirFd>,
}

impl DirFd {
    /// Acquire an EXCLUSIVE advisory lock on this directory, blocking until no
    /// other pin holds one. A caller staging into a shared destination tree
    /// holds this for the whole enumerate/cleanup/copy/publish sequence, so a
    /// concurrent attempt can neither remove the first attempt's live temp
    /// tree nor interleave its publishes. Works on Linux and macOS (flock on a
    /// directory fd); stale temp-tree cleanup is safe only once this lock is
    /// held — a free lock proves the creating process is gone.
    pub fn lock_exclusive(&self) -> Result<DirLock<'_>, FdError> {
        // A dup is an independent open description: it CONTENDS with another
        // process's pin (unlike a borrowed raw fd, which would describe the
        // same lock owner).
        let pin = self
            .file
            .try_clone()
            .map_err(|error| FdError::new("<flock>", FdErrorKind::Other(error.to_string())))?;
        match Flock::lock(pin, FlockArg::LockExclusive) {
            Ok(held) => Ok(DirLock {
                held,
                _pin: std::marker::PhantomData,
            }),
            Err((_pin, error)) => Err(FdError::new(
                "<flock>",
                FdErrorKind::Other(error.to_string()),
            )),
        }
    }
}

/// Failures from an fd walk. Messages name the offending component.
#[derive(Debug)]
pub struct FdError {
    /// Single-component name (or display path) the walk rejected.
    pub at: String,
    /// Why it was rejected.
    pub kind: FdErrorKind,
}

/// Why an fd walk failed.
#[derive(Debug, PartialEq, Eq)]
pub enum FdErrorKind {
    /// The component is a symlink and `O_NOFOLLOW` refused it.
    Symlink,
    /// A path component existed and was not a directory.
    NotDirectory,
    /// The component does not exist.
    Missing,
    /// A name was empty, self/parent, or contained a separator.
    BadComponent,
    /// The regular leaf has more than one hard link (`st_nlink > 1`): appending
    /// would also write through every other directory entry that shares the
    /// inode, possibly one outside the pinned root.
    Hardlinked,
    /// Any other OS error (the raw message is kept).
    Other(String),
}

impl FdError {
    /// Construct a walk failure from its component and kind.
    pub fn new(at: impl Into<String>, kind: FdErrorKind) -> Self {
        Self {
            at: at.into(),
            kind,
        }
    }

    fn io(at: &[u8], error: Errno) -> Self {
        let at = String::from_utf8_lossy(at).into_owned();
        let kind = match error {
            Errno::ELOOP => FdErrorKind::Symlink,
            Errno::ENOTDIR => FdErrorKind::NotDirectory,
            Errno::ENOENT => FdErrorKind::Missing,
            other => FdErrorKind::Other(other.to_string()),
        };
        Self { at, kind }
    }

    /// Map a nix `Errno` (from an openat on a canonicalized path) to a kind.
    fn from_errno(at: impl Into<String>, error: Errno) -> Self {
        let kind = match error {
            Errno::ELOOP => FdErrorKind::Symlink,
            Errno::ENOTDIR => FdErrorKind::NotDirectory,
            Errno::ENOENT => FdErrorKind::Missing,
            other => FdErrorKind::Other(other.to_string()),
        };
        Self {
            at: at.into(),
            kind,
        }
    }

    /// Whether the walk hit a symlink component.
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.kind == FdErrorKind::Symlink
    }

    /// Whether the underlying OS error was "already exists" (used to retry a
    /// randomized exclusive allocation).
    #[must_use]
    pub fn is_already_exists(&self) -> bool {
        matches!(self.kind, FdErrorKind::Other(ref msg) if msg.contains("EEXIST"))
    }
}

impl std::fmt::Display for FdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            FdErrorKind::Symlink => write!(f, "{:?} is a symlink; refusing to follow it", self.at),
            FdErrorKind::NotDirectory => write!(f, "{:?} is not a directory", self.at),
            FdErrorKind::Missing => write!(f, "{:?} does not exist", self.at),
            FdErrorKind::BadComponent => {
                write!(f, "{:?} is not a single safe path component", self.at)
            }
            FdErrorKind::Hardlinked => {
                write!(
                    f,
                    "{:?} is hard-linked (nlink > 1); refusing to append to a shared inode",
                    self.at
                )
            }
            FdErrorKind::Other(ref message) => write!(f, "{:?}: {message}", self.at),
        }
    }
}

impl std::error::Error for FdError {}

impl From<FdError> for std::io::Error {
    fn from(error: FdError) -> Self {
        let kind = match error.kind {
            FdErrorKind::Symlink | FdErrorKind::NotDirectory | FdErrorKind::Hardlinked => {
                std::io::ErrorKind::PermissionDenied
            }
            FdErrorKind::Missing => std::io::ErrorKind::NotFound,
            FdErrorKind::BadComponent => std::io::ErrorKind::InvalidInput,
            FdErrorKind::Other(_) => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, error.to_string())
    }
}

/// Reject anything that is not exactly one opaque path component: no
/// separators, no NUL, no `.`/`..`.
fn check_component(name: &[u8]) -> Result<(), FdError> {
    if name.is_empty()
        || name == b"."
        || name == b".."
        || name.iter().any(|&byte| byte == b'/' || byte == 0)
    {
        Err(FdError::new(
            String::from_utf8_lossy(name),
            FdErrorKind::BadComponent,
        ))
    } else {
        Ok(())
    }
}

/// Wrap a raw fd returned by nix as an owned [`File`] (nix does not close it).
unsafe fn own_fd(fd: RawFd) -> File {
    unsafe { File::from_raw_fd(fd) }
}

/// Exact S_IFMT classification: `(mode & S_IFMT) == <type>`. A bitmask test
/// like `(mode & S_IFREG) == S_IFREG` falsely matches sockets (0140000) and
/// macOS whiteouts; this never does.
fn classify(stat: &FileStat) -> (LeafKind, u64) {
    let mode = stat.st_mode;
    let ifmt = mode & nix::libc::S_IFMT;
    let kind = if ifmt == nix::libc::S_IFLNK {
        LeafKind::Symlink
    } else if ifmt == nix::libc::S_IFREG {
        LeafKind::Regular
    } else if ifmt == nix::libc::S_IFDIR {
        LeafKind::Directory
    } else {
        LeafKind::Other
    };
    // `st_dev`/`st_size` are already u64 on linux-gnu but narrower on macOS,
    // where the cast is required.
    #[allow(clippy::unnecessary_cast)]
    let len = stat.st_size as u64;
    (kind, len)
}

fn fstat_fd(fd: RawFd) -> Result<FileStat, FdError> {
    nix::sys::stat::fstat(fd)
        .map_err(|error| FdError::new("<fd>", FdErrorKind::Other(error.to_string())))
}

#[allow(clippy::unnecessary_cast)]
fn dev_of(stat: &FileStat) -> u64 {
    stat.st_dev as u64
}

#[allow(clippy::unnecessary_cast)]
fn ino_of(stat: &FileStat) -> u64 {
    stat.st_ino as u64
}

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
// `O_NONBLOCK` matters for leaves: without it, opening a FIFO `O_RDONLY`
// (or `O_WRONLY`) blocks until the other end appears. Every leaf open below
// includes it, and the fstatat/fstat checks still require a regular file, so
// a stat→open swap to a FIFO can neither block nor be read.
const LEAF_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC)
    .union(OFlag::O_NONBLOCK);
const APPEND_FLAGS: OFlag = OFlag::O_WRONLY
    .union(OFlag::O_APPEND)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC)
    .union(OFlag::O_NONBLOCK);
const CREATE_EXCL_FLAGS: OFlag = OFlag::O_WRONLY
    .union(OFlag::O_CREAT)
    .union(OFlag::O_EXCL)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);
const DIR_MODE: Mode = Mode::from_bits_truncate(0o700);
const FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

impl DirFd {
    /// Pin a TRUSTED ROOT that already exists.
    ///
    /// The whole path must resolve to a REAL DIRECTORY. The final component is
    /// checked with `fstatat(AT_SYMLINK_NOFOLLOW)`: a symlink-to-directory is
    /// REFUSED (it climbs to its parent, sees the path is not itself a real
    /// dir, and returns an error) — so this never pins a descendant symlink
    /// like `home/projects -> /outside`. Intermediate SYSTEM mount symlinks
    /// above the root (macOS `/tmp → /private/tmp`, `/var → /private/var`) are
    /// the one thing resolved: the deepest real-directory ancestor is
    /// realpath'd once and opened `O_NOFOLLOW|O_DIRECTORY`. Callers pin only a
    /// configured base/home and then walk every descendant with
    /// [`Self::subpath`] / [`Self::ensure_subpath`].
    pub fn anchor_existing(path: &Path) -> Result<Self, FdError> {
        if !path.is_absolute() {
            return Err(FdError::new(
                path.display().to_string(),
                FdErrorKind::BadComponent,
            ));
        }
        let mut leaf_error: Option<FdErrorKind> = None;
        // Climb to the deepest ancestor whose FINAL component is a real
        // directory under lstat (no trailing symlink).
        let mut current = path.to_path_buf();
        let root = loop {
            match nix::sys::stat::fstatat(
                Some(nix::libc::AT_FDCWD),
                &current,
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) if (stat.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFDIR => {
                    break current;
                }
                Ok(stat) if (stat.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFLNK => {
                    leaf_error = Some(FdErrorKind::Symlink);
                }
                Ok(_) => leaf_error = Some(FdErrorKind::NotDirectory),
                Err(Errno::ENOENT) => leaf_error = Some(FdErrorKind::Missing),
                Err(error) => leaf_error = Some(FdErrorKind::Other(error.to_string())),
            }
            let Some(parent) = current.parent().filter(|p| *p != current) else {
                return Err(FdError::new(
                    path.display().to_string(),
                    FdErrorKind::BadComponent,
                ));
            };
            current = parent.to_path_buf();
        };
        if root != path {
            // The requested path itself was not a real directory (a trailing
            // symlink, special file, or missing entry). Refuse rather than
            // pinning its parent: callers use subpath() to reach descendants.
            return Err(FdError::new(
                path.display().to_string(),
                leaf_error.unwrap_or(FdErrorKind::NotDirectory),
            ));
        }
        let canonical = std::fs::canonicalize(&root)
            .map_err(|error| FdError::new(root.display().to_string(), errno_kind(error)))?;
        // Round 7 item 2: the lstat loop above only refuses a symlink as the
        // FINAL component; a real dir found through an intermediate link
        // canonicalizes past it. Refuse any divergence a system mount does not
        // account for.
        let canonical = require_system_mount_divergence_only(&root, canonical)?;
        let fd = nix::fcntl::openat(
            Some(nix::libc::AT_FDCWD),
            &canonical,
            DIR_FLAGS,
            Mode::empty(),
        )
        .map_err(|error| FdError::from_errno(canonical.display().to_string(), error))?;
        Ok(Self {
            file: unsafe { own_fd(fd) },
        })
    }

    /// Pin a trusted root, creating its 0700 tail if absent.
    ///
    /// The deepest EXISTING ancestor whose final component is a REAL DIRECTORY
    /// (lstat, a trailing symlink refused) is realpath'd once (crossing only
    /// system mount symlinks); every component of the remaining tail is
    /// created `mkdirat` + re-opened `O_NOFOLLOW`. A trailing symlink is not
    /// created through.
    pub fn anchor_or_create(path: &Path) -> Result<Self, FdError> {
        if !path.is_absolute() {
            return Err(FdError::new(
                path.display().to_string(),
                FdErrorKind::BadComponent,
            ));
        }
        // Deepest existing REAL directory (lstat), collecting the tail.
        let mut existing = path.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            let is_real_dir = match nix::sys::stat::fstatat(
                Some(nix::libc::AT_FDCWD),
                &existing,
                AtFlags::AT_SYMLINK_NOFOLLOW,
            ) {
                Ok(stat) => (stat.st_mode & nix::libc::S_IFMT) == nix::libc::S_IFDIR,
                Err(Errno::ENOENT) => false,
                Err(_) => false,
            };
            if is_real_dir {
                break;
            }
            let name = match existing.file_name().map(std::ffi::OsStr::to_owned) {
                Some(name) => name,
                None => {
                    return Err(FdError::new(
                        path.display().to_string(),
                        FdErrorKind::BadComponent,
                    ));
                }
            };
            tail.push(name);
            let Some(parent) = existing.parent().filter(|p| *p != existing) else {
                return Err(FdError::new(
                    path.display().to_string(),
                    FdErrorKind::BadComponent,
                ));
            };
            existing = parent.to_path_buf();
        }
        let canonical = std::fs::canonicalize(&existing)
            .map_err(|error| FdError::new(existing.display().to_string(), errno_kind(error)))?;
        // Round 7 item 2: a real-directory verdict only lstat()s the FINAL
        // component — an existing ancestor reached THROUGH an intermediate link
        // (`<R>/ev -> /outside`, anchoring `<R>/ev/sub`) canonicalizes past the
        // link. The tail is walked O_NOFOLLOW, but the anchor itself must never
        // pin the link target. System mount symlinks (macOS /tmp, /var) are the
        // only permitted divergence.
        let canonical = require_system_mount_divergence_only(&existing, canonical)?;
        let mut current = Self {
            file: unsafe {
                own_fd(
                    nix::fcntl::openat(
                        Some(nix::libc::AT_FDCWD),
                        &canonical,
                        DIR_FLAGS,
                        Mode::empty(),
                    )
                    .map_err(|error| FdError::from_errno(canonical.display().to_string(), error))?,
                )
            },
        };
        for part in tail.into_iter().rev() {
            current = current.ensure_subdir(part.as_bytes())?;
        }
        Ok(current)
    }

    /// Whether this opened directory is world-writable (`S_IWOTH`), read from
    /// the opened fd — no path re-stat.
    pub fn is_world_writable(&self) -> Result<bool, FdError> {
        let stat = fstat_fd(self.as_raw_fd())?;
        Ok(stat.st_mode & nix::libc::S_IWOTH != 0)
    }

    /// Create a fresh 0700 subdirectory with `mkdirat`; unlike
    /// [`Self::ensure_subdir`], an existing name is an error (`EEXIST`), so a
    /// randomized allocation cannot adopt or delete a directory it did not
    /// create.
    pub fn create_subdir_excl(&self, name: &[u8]) -> Result<Self, FdError> {
        check_component(name)?;
        match nix::sys::stat::mkdirat(Some(self.as_raw_fd()), name, DIR_MODE) {
            // The caller (a randomized allocation) retries with another name.
            Err(Errno::EEXIST) => Err(FdError::new(
                String::from_utf8_lossy(name),
                FdErrorKind::Other("EEXIST: name taken".into()),
            )),
            Err(error) => Err(FdError::io(name, error)),
            Ok(()) => self.subdir(name),
        }
    }

    /// Owner uid of the opened directory (callers compare against `geteuid`).
    pub fn uid(&self) -> Result<u32, FdError> {
        let stat = fstat_fd(self.as_raw_fd())?;
        #[allow(clippy::unnecessary_cast)]
        Ok(stat.st_uid as u32)
    }

    /// Mode bits of the opened directory.
    pub fn mode(&self) -> Result<u32, FdError> {
        let stat = fstat_fd(self.as_raw_fd())?;
        #[allow(clippy::unnecessary_cast)]
        Ok(stat.st_mode as u32)
    }

    /// Hard-link count of the opened directory.
    pub fn nlink(&self) -> Result<u64, FdError> {
        let stat = fstat_fd(self.as_raw_fd())?;
        #[allow(clippy::unnecessary_cast)]
        Ok(stat.st_nlink as u64)
    }

    /// The `(st_dev, st_ino)` pair of this directory fd.
    pub fn dir_identity(&self) -> Result<(u64, u64), FdError> {
        let stat = fstat_fd(self.as_raw_fd())?;
        Ok((dev_of(&stat), ino_of(&stat)))
    }

    /// Open a duplicate fd on this same directory ("."), used to obtain an
    /// owned [`DirFd`] without consuming `self` at the start of a walk.
    fn self_dir(&self) -> Result<Self, FdError> {
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), &b"."[..], DIR_FLAGS, Mode::empty())
            .map_err(|error| FdError::io(b".", error))?;
        Ok(Self {
            file: unsafe { own_fd(fd) },
        })
    }

    /// Walk an existing relative `path` below this pinned anchor; every hop is
    /// `O_NOFOLLOW|O_DIRECTORY`. `.`/`..` components and absolute/root
    /// prefixes are refused, so the result can never leave the anchor.
    pub fn subpath(&self, path: &Path) -> Result<Self, FdError> {
        let mut current = self.self_dir()?;
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    current = current.subdir(part.as_bytes())?;
                }
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => {
                    return Err(FdError::new(
                        path.display().to_string(),
                        FdErrorKind::BadComponent,
                    ));
                }
            }
        }
        Ok(current)
    }

    /// Walk/create a relative `path` below this pinned anchor.
    pub fn ensure_subpath(&self, path: &Path) -> Result<Self, FdError> {
        let mut current = self.self_dir()?;
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    current = current.ensure_subdir(part.as_bytes())?;
                }
                Component::CurDir
                | Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_) => {
                    return Err(FdError::new(
                        path.display().to_string(),
                        FdErrorKind::BadComponent,
                    ));
                }
            }
        }
        Ok(current)
    }

    /// Open one existing subdirectory component (`O_NOFOLLOW|O_DIRECTORY`).
    ///
    /// A pre-open `fstatat` gives the precise error (a link to a directory
    /// can surface as `ENOTDIR` rather than `ELOOP` once `O_DIRECTORY` is in
    /// the flag set); the `O_NOFOLLOW` open still closes the stat→open race.
    pub fn subdir(&self, name: &[u8]) -> Result<Self, FdError> {
        check_component(name)?;
        if let Some(entry) = self.classify_leaf(name)? {
            match entry.kind {
                LeafKind::Directory => {}
                LeafKind::Symlink => {
                    return Err(FdError::new(
                        String::from_utf8_lossy(name),
                        FdErrorKind::Symlink,
                    ));
                }
                LeafKind::Regular | LeafKind::Other => {
                    return Err(FdError::new(
                        String::from_utf8_lossy(name),
                        FdErrorKind::NotDirectory,
                    ));
                }
            }
        }
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), name, DIR_FLAGS, Mode::empty())
            .map_err(|error| FdError::io(name, error))?;
        Ok(Self {
            file: unsafe { own_fd(fd) },
        })
    }

    /// Open one subdirectory component, creating it with 0700 if absent. An
    /// existing component that is not a real directory (a symlink included) is
    /// refused: the post-`mkdirat` open is an independent syscall, so racing
    /// the create with a symlink swap still fails `O_NOFOLLOW`.
    pub fn ensure_subdir(&self, name: &[u8]) -> Result<Self, FdError> {
        check_component(name)?;
        match nix::sys::stat::mkdirat(Some(self.as_raw_fd()), name, DIR_MODE) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(error) => return Err(FdError::io(name, error)),
        }
        self.subdir(name)
    }

    /// Classify one leaf without following it (`fstatat(AT_SYMLINK_NOFOLLOW)`).
    /// `Ok(None)` means the component does not exist.
    pub fn classify_leaf(&self, name: &[u8]) -> Result<Option<Entry>, FdError> {
        check_component(name)?;
        match nix::sys::stat::fstatat(Some(self.as_raw_fd()), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                let (kind, len) = classify(&stat);
                Ok(Some(Entry {
                    name: name.to_vec(),
                    kind,
                    len,
                }))
            }
            Err(Errno::ENOENT) => Ok(None),
            Err(error) => Err(FdError::io(name, error)),
        }
    }

    /// Open a leaf `O_NOFOLLOW` and classify the **opened fd** with `fstat`.
    ///
    /// The entry is `fstatat`-classified BEFORE the open: a FIFO/device is
    /// never even opened (`O_NONBLOCK` in the flags is the second line of
    /// defence), and the post-open `fstat` confirms the fd itself.
    pub fn open_leaf(&self, name: &[u8]) -> Result<OpenedLeaf, FdError> {
        check_component(name)?;
        let pre = self
            .classify_leaf(name)?
            .ok_or_else(|| FdError::io(name, Errno::ENOENT))?;
        if !matches!(pre.kind, LeafKind::Regular | LeafKind::Other) {
            return Err(FdError::new(
                String::from_utf8_lossy(name),
                match pre.kind {
                    LeafKind::Symlink => FdErrorKind::Symlink,
                    LeafKind::Directory => FdErrorKind::NotDirectory,
                    LeafKind::Other | LeafKind::Regular => unreachable!(),
                },
            ));
        }
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), name, LEAF_FLAGS, Mode::empty())
            .map_err(|error| FdError::io(name, error))?;
        let file = unsafe { own_fd(fd) };
        let stat = fstat_fd(file.as_raw_fd())?;
        let (kind, len) = classify(&stat);
        Ok(OpenedLeaf { file, kind, len })
    }

    /// Open a leaf and require it to be a real regular file on the opened fd.
    /// Symlinks never reach here (`O_NOFOLLOW`), and a FIFO/device/socket is
    /// refused rather than read or blocked on.
    pub fn open_regular_leaf(&self, name: &[u8]) -> Result<OpenedLeaf, FdError> {
        let leaf = self.open_leaf(name)?;
        match leaf.kind {
            LeafKind::Regular => Ok(leaf),
            other => Err(FdError::new(
                String::from_utf8_lossy(name),
                match other {
                    LeafKind::Symlink => FdErrorKind::Symlink,
                    LeafKind::Directory => FdErrorKind::NotDirectory,
                    LeafKind::Other => FdErrorKind::Other("not a regular file".into()),
                    LeafKind::Regular => unreachable!(),
                },
            )),
        }
    }

    /// Create a leaf with `O_EXCL` (creation fails if it already exists),
    /// 0600, `O_NOFOLLOW`.
    pub fn create_leaf_excl(&self, name: &[u8]) -> Result<File, FdError> {
        check_component(name)?;
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), name, CREATE_EXCL_FLAGS, FILE_MODE)
            .map_err(|error| FdError::io(name, error))?;
        Ok(unsafe { own_fd(fd) })
    }

    /// Open an existing regular leaf for appending
    /// (`O_WRONLY|O_APPEND|O_NOFOLLOW|O_NONBLOCK`).
    ///
    /// The entry is `fstatat`-classified first, so a FIFO never blocks the
    /// open; the post-open `fstat` requires a regular file on the fd with
    /// exactly ONE link. Absence is [`FdErrorKind::Missing`], a symlink is
    /// [`FdErrorKind::Symlink`], and a hard-linked leaf (`st_nlink > 1`) is
    /// [`FdErrorKind::Hardlinked`]: appending would also grow every other
    /// directory entry sharing the inode — round 7 item 4, the fake-harness
    /// confinement bypass where an operator file was hardlinked into an
    /// allocated home at `projects/<slug>/<S>.jsonl`.
    pub fn open_append_leaf(&self, name: &[u8]) -> Result<File, FdError> {
        check_component(name)?;
        let pre = self
            .classify_leaf(name)?
            .ok_or_else(|| FdError::io(name, Errno::ENOENT))?;
        if pre.kind != LeafKind::Regular {
            return Err(FdError::new(
                String::from_utf8_lossy(name),
                match pre.kind {
                    LeafKind::Symlink => FdErrorKind::Symlink,
                    LeafKind::Directory => FdErrorKind::NotDirectory,
                    LeafKind::Other => FdErrorKind::Other("not a regular file".into()),
                    LeafKind::Regular => unreachable!(),
                },
            ));
        }
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), name, APPEND_FLAGS, FILE_MODE)
            .map_err(|error| FdError::io(name, error))?;
        let file = unsafe { own_fd(fd) };
        let stat = fstat_fd(file.as_raw_fd())?;
        if classify(&stat).0 != LeafKind::Regular {
            return Err(FdError::new(
                String::from_utf8_lossy(name),
                FdErrorKind::Other("not a regular file".into()),
            ));
        }
        // Round 7 item 4: a single-link count on the OPENED fd. A hardlinked
        // transcript shares the inode with a path outside the pinned root; an
        // append would write the child's turn through that entry too.
        if stat.st_nlink != 1 {
            return Err(FdError::new(
                String::from_utf8_lossy(name),
                FdErrorKind::Hardlinked,
            ));
        }
        Ok(file)
    }

    /// Rename one component relative to two pinned directory fds. The caller
    /// must classify the destination name first: `renameat` overwrites an
    /// existing regular file, but replacing a directory or symlink is refused.
    pub fn rename(&self, from: &[u8], to_dir: &DirFd, to: &[u8]) -> Result<(), FdError> {
        check_component(from)?;
        check_component(to)?;
        nix::fcntl::renameat(Some(self.as_raw_fd()), from, Some(to_dir.as_raw_fd()), to)
            .map_err(|error| FdError::io(to, error))
    }

    /// Remove a regular-file (or other non-directory) component.
    pub fn unlink_file(&self, name: &[u8]) -> Result<(), FdError> {
        check_component(name)?;
        nix::unistd::unlinkat(
            Some(self.as_raw_fd()),
            name,
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        )
        .map_err(|error| FdError::io(name, error))
    }

    /// Remove an empty directory component.
    pub fn unlink_dir(&self, name: &[u8]) -> Result<(), FdError> {
        check_component(name)?;
        nix::unistd::unlinkat(
            Some(self.as_raw_fd()),
            name,
            nix::unistd::UnlinkatFlags::RemoveDir,
        )
        .map_err(|error| FdError::io(name, error))
    }

    /// List immediate children, classified without following links.
    pub fn entries(&self) -> Result<Vec<Entry>, FdError> {
        let mut dir = Dir::openat(Some(self.as_raw_fd()), &b"."[..], DIR_FLAGS, Mode::empty())
            .map_err(|error| FdError::io(b".", error))?;
        let mut out = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(|error| FdError::io(b".", error))?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if let Some(leaf) = self.classify_leaf(bytes)? {
                out.push(leaf);
            }
        }
        Ok(out)
    }

    /// Recursively verify that NO symlink (or other non-directory/non-regular
    /// entry) exists anywhere below this directory: every descendant must be a
    /// real directory or regular file, reached link-free. Unlike a check of
    /// just the immediate children, this catches a NESTED link such as
    /// `<S>/subagents -> /outside` or `memory/MEMORY.md -> /outside`, which is
    /// invisible to a root-only check but would redirect a later relative walk.
    /// Depth-bounded like every other walk here; the first offending entry is
    /// returned as a [`FdErrorKind::Symlink`] (other special files as
    /// [`FdErrorKind::Other`]).
    pub fn reject_symlink_descendants(&self, max_depth: u32) -> Result<(), FdError> {
        self.reject_links_below("", max_depth)
    }

    fn reject_links_below(&self, rel: &str, depth_left: u32) -> Result<(), FdError> {
        for entry in self.entries()? {
            match entry.kind {
                LeafKind::Directory => {
                    if depth_left == 0 {
                        return Err(FdError::new(
                            format!("{rel}/{}", String::from_utf8_lossy(&entry.name)),
                            FdErrorKind::Other("sidecar tree deeper than the staging limit".into()),
                        ));
                    }
                    let sub = self.subdir(&entry.name)?;
                    let child_rel = if rel.is_empty() {
                        String::from_utf8_lossy(&entry.name).into_owned()
                    } else {
                        format!("{rel}/{}", String::from_utf8_lossy(&entry.name))
                    };
                    sub.reject_links_below(&child_rel, depth_left - 1)?;
                }
                LeafKind::Regular => {}
                LeafKind::Symlink => {
                    return Err(FdError::new(
                        format!("{rel}/{}", String::from_utf8_lossy(&entry.name)),
                        FdErrorKind::Symlink,
                    ));
                }
                LeafKind::Other => {
                    return Err(FdError::new(
                        format!("{rel}/{}", String::from_utf8_lossy(&entry.name)),
                        FdErrorKind::Other("not a regular file or directory".into()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Recursively remove a subtree directly below this fd. Used ONLY on
    /// private temp trees the caller created with [`Self::ensure_subdir`], so
    /// every removal stays descriptor-relative and symlink-safe (a symlink
    /// inside the temp tree is unlinked as a link, its target never touched).
    pub fn remove_private_tree(&self, name: &[u8]) -> Result<(), FdError> {
        check_component(name)?;
        let subtree = match self.subdir(name) {
            Ok(dir) => dir,
            Err(FdError {
                kind: FdErrorKind::Missing,
                ..
            }) => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in subtree.entries()? {
            match entry.kind {
                LeafKind::Directory => subtree.remove_private_tree(&entry.name)?,
                // Links are unlinked as files; the target is never touched.
                LeafKind::Regular | LeafKind::Symlink | LeafKind::Other => {
                    if let Err(FdError {
                        kind: FdErrorKind::Missing,
                        ..
                    }) = subtree.unlink_file(&entry.name)
                    {}
                }
            }
        }
        match self.unlink_dir(name) {
            Ok(())
            | Err(FdError {
                kind: FdErrorKind::Missing,
                ..
            }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn errno_kind(error: std::io::Error) -> FdErrorKind {
    if error.kind() == std::io::ErrorKind::NotFound {
        FdErrorKind::Missing
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        FdErrorKind::Symlink
    } else {
        FdErrorKind::Other(error.to_string())
    }
}

/// Whether the only difference between a LEXICAL anchor path and its
/// canonicalised (physical) form is a system mount symlink ABOVE the anchor —
/// `/tmp → /private/tmp` and `/var → /private/var` on macOS. Such a link is
/// part of the OS layout, never an entry an adversary controls, so it is the
/// one class of divergence an anchor is allowed to cross.
///
/// Round 7 item 2: without this check `anchor_or_create`/`anchor_existing`
/// canonicalized a real directory found BELOW an untrusted intermediate link
/// (`<R>/ev -> /outside`, a real `/outside/sub`): `fstatat(NOFOLLOW)` only
/// refuses the FINAL component, so the anchor climbed to and pinned
/// `/outside/sub`. Any other lexical/physical mismatch is a symlink in the
/// tree the caller asked to pin and is refused.
fn divergence_is_only_a_system_mount(lexical: &Path, canonical: &Path) -> bool {
    if lexical == canonical {
        return true;
    }
    /// Strip a prefix and return the remaining tail, component-exact.
    fn tail_after<'a>(path: &'a Path, prefix: &Path) -> Option<&'a Path> {
        path.strip_prefix(prefix).ok()
    }
    // (lexical prefix, physical counterpart) — macOS mounts /tmp and /var as
    // symlinks into /private. On Linux both are real, the pairs simply never
    // match a divergence.
    const MOUNT_ALIASES: &[(&str, &str)] = &[("/tmp", "/private/tmp"), ("/var", "/private/var")];
    for &(lex_prefix, can_prefix) in MOUNT_ALIASES {
        if let (Some(lex_tail), Some(can_tail)) = (
            tail_after(lexical, Path::new(lex_prefix)),
            tail_after(canonical, Path::new(can_prefix)),
        ) && lex_tail == can_tail
        {
            return true;
        }
    }
    false
}

/// Refuse a canonicalised anchor whose physical path diverges from its lexical
/// path through anything but a system mount symlink (round 7 item 2). Returns
/// the canonical path on success.
fn require_system_mount_divergence_only(
    lexical: &Path,
    canonical: PathBuf,
) -> Result<PathBuf, FdError> {
    if divergence_is_only_a_system_mount(lexical, &canonical) {
        Ok(canonical)
    } else {
        Err(FdError::new(
            lexical.display().to_string(),
            FdErrorKind::Symlink,
        ))
    }
}

/// Copy at most `cap` bytes from `src` to `dst`, failing the moment one more
/// byte would cross the cap. The caller supplies the hashing writer so
/// provenance hashes the bytes ACTUALLY copied. Callers that enumerated a
/// file earlier should pass the REMAINING aggregate budget (not the file's
/// recorded length) so a file that grows after enumeration cannot push the
/// total copy over the cap; the actual number of bytes copied is returned
/// and must be what the caller charges.
pub fn copy_capped<R, W>(src: &mut R, dst: &mut W, cap: u64) -> std::io::Result<u64>
where
    R: Read + ?Sized,
    W: Write + ?Sized,
{
    let mut buf = vec![0_u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let max_remaining = usize::try_from(cap.saturating_sub(total)).unwrap_or(usize::MAX);
        if max_remaining == 0 {
            // Probe for one extra byte: EOF means the copy is exactly at the
            // cap; any byte means it would exceed it.
            let extra = src.read(&mut buf[..1])?;
            if extra == 0 {
                dst.flush()?;
                return Ok(total);
            }
            return Err(std::io::Error::other(format!(
                "resume staging size limit exceeded: more than {cap} bytes"
            )));
        }
        let read = src.take(max_remaining as u64).read(&mut buf)?;
        if read == 0 {
            dst.flush()?;
            return Ok(total);
        }
        let next = total
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("byte count overflow while staging"))?;
        if next > cap {
            return Err(std::io::Error::other(format!(
                "resume staging size limit exceeded: more than {cap} bytes"
            )));
        }
        dst.write_all(&buf[..read])?;
        total = next;
    }
}

/// A writer that fans every byte out to two writers (the file and a hash),
/// so a copy can be hashed without a second read of the source.
pub struct TeeWriter<'a, R, W> {
    first: &'a mut R,
    second: &'a mut W,
}

impl<'a, R: Write, W: Write> TeeWriter<'a, R, W> {
    /// Fan writes into `first` and `second`.
    pub fn new(first: &'a mut R, second: &'a mut W) -> Self {
        Self { first, second }
    }
}

impl<R: Write, W: Write> Write for TeeWriter<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.first.write_all(buf)?;
        self.second.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.first.flush()?;
        self.second.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Randomised, exclusive, short-lived 0700 scratch dir with a SHORT path
    /// (so a unix socket bound under it stays inside `sun_path` on macOS) and
    /// Drop cleanup. Never a predictable `mkdir -p` name: an existing entry is
    /// never adopted or deleted.
    struct Tmp {
        path: std::path::PathBuf,
    }
    impl Tmp {
        fn new() -> Tmp {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            for _ in 0..16 {
                let name = format!(
                    "f{:x}{:x}",
                    std::process::id() ^ SEQ.fetch_add(1, Ordering::Relaxed) as u32,
                    nanos
                );
                let path = std::path::PathBuf::from("/tmp").join(&name);
                use std::os::unix::fs::DirBuilderExt;
                if std::fs::DirBuilder::new()
                    .recursive(false)
                    .mode(0o700)
                    .create(&path)
                    .is_ok()
                {
                    return Tmp { path };
                }
            }
            panic!("could not allocate a unique short scratch dir under /tmp");
        }
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> Tmp {
        Tmp::new()
    }

    fn anchor(tmp: &Tmp) -> DirFd {
        DirFd::anchor_existing(tmp.path()).expect("anchor")
    }

    #[test]
    fn anchor_refuses_a_trailing_symlink_to_directory() {
        // Round 5 part 2: even though the symlink target is a real directory,
        // anchoring the LINK path must fail (never resolve into it) —
        // `home/projects -> /outside` is the production attack.
        let tmp = tempdir();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let projects = tmp.path().join("projects");
        std::os::unix::fs::symlink(&outside, &projects).expect("projects link");
        let error = DirFd::anchor_existing(&projects).expect_err("symlink anchor refused");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        // anchor_or_create on the same link must not create through it.
        let error2 = DirFd::anchor_or_create(&projects).expect_err("symlink anchor-create refused");
        assert_eq!(error2.kind, FdErrorKind::Symlink);
        // And nothing was written into the outside target.
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        // The parent remains pinnable and reaches the link only as an error.
        let home = anchor(&tmp);
        assert!(matches!(
            home.subdir(b"projects").err().map(|e| e.kind),
            Some(FdErrorKind::Symlink)
        ));
    }

    #[test]
    fn walks_real_components_and_creates_children() {
        let tmp = tempdir();
        let sub = anchor(&tmp).ensure_subdir(b"projects").expect("mkdir");
        let mut file = sub.create_leaf_excl(b"a.jsonl").expect("excl create");
        file.write_all(b"{}").expect("write");
        let leaf = sub.open_regular_leaf(b"a.jsonl").expect("regular leaf");
        assert_eq!(leaf.kind, LeafKind::Regular);
        assert_eq!(leaf.len, 2);
    }

    #[test]
    fn anchor_crosses_a_system_mount_symlink_but_rejects_links_below() {
        // /tmp is a symlink to /private/tmp on macOS and real on Linux; the
        // anchor resolves it either way.
        let tmp = tempdir();
        let project = &tmp.path().join("real-dir");
        std::fs::create_dir_all(project).expect("mkdir");
        use std::os::unix::fs::symlink;
        symlink(project.as_path(), tmp.path().join("linkdir")).expect("symlink dir");
        assert!(anchor(&tmp).subdir(b"linkdir").is_err());
        // A multi-component walk refuses parent/root components.
        assert!(matches!(
            anchor(&tmp)
                .ensure_subpath(std::path::Path::new("a/../../x"))
                .err()
                .map(|e| e.kind),
            Some(FdErrorKind::BadComponent)
        ));
        assert!(matches!(
            anchor(&tmp)
                .ensure_subpath(std::path::Path::new("/etc"))
                .err()
                .map(|e| e.kind),
            Some(FdErrorKind::BadComponent)
        ));
    }

    #[test]
    fn a_fifo_is_classified_other_and_never_opened() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        let fifo = &tmp.path().join("pipe");
        nix::unistd::mkfifo(fifo.as_os_str(), Mode::from_bits_truncate(0o600)).expect("mkfifo");
        let entry = root.classify_leaf(b"pipe").expect("stat").expect("entry");
        assert_eq!(entry.kind, LeafKind::Other);
        let error = root.open_regular_leaf(b"pipe").expect_err("fifo refused");
        assert!(matches!(error.kind, FdErrorKind::Other(_)));
    }

    /// Opening a FIFO for append must return promptly instead of blocking:
    /// the fstatat pre-classification rejects it before any open.
    #[test]
    fn open_append_leaf_never_blocks_on_a_fifo() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        nix::unistd::mkfifo(
            tmp.path().join("hose").as_os_str(),
            Mode::from_bits_truncate(0o600),
        )
        .expect("mkfifo");
        let started = std::time::Instant::now();
        let result = root.open_append_leaf(b"hose");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "append blocked"
        );
        assert!(result.is_err(), "fifo append refused");
    }

    /// Round 7 item 4: a regular leaf with more than one hard link must not be
    /// opened for append — the second directory entry sharing the inode may
    /// sit outside the pinned root, so an append would write through it too.
    #[cfg(unix)]
    #[test]
    fn open_append_leaf_refuses_a_hardlinked_leaf() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        std::fs::write(tmp.path().join("inside.jsonl"), b"SHARED\n").expect("seed");
        // The second name lives OUTSIDE the pinned tree, exactly like an
        // operator file hardlinked into an allocated fake home.
        let outside_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&outside_dir).expect("outside dir");
        std::fs::hard_link(
            tmp.path().join("inside.jsonl"),
            outside_dir.join("operator.jsonl"),
        )
        .expect("hardlink");
        let error = root
            .open_append_leaf(b"inside.jsonl")
            .expect_err("a hardlinked leaf is refused for append");
        assert_eq!(error.kind, FdErrorKind::Hardlinked);
    }

    /// A freshly created single-link leaf appends normally: the link-count
    /// leg refuses only shared inodes.
    #[test]
    fn open_append_leaf_accepts_a_single_link_leaf() {
        use std::io::Write;
        let tmp = tempdir();
        let root = anchor(&tmp);
        let mut file = root.create_leaf_excl(b"single.jsonl").expect("excl create");
        file.write_all(b"one\n").expect("seed");
        drop(file);
        let mut append = root
            .open_append_leaf(b"single.jsonl")
            .expect("single link appends");
        append.write_all(b"two\n").expect("append");
        drop(append);
        assert_eq!(
            std::fs::read(tmp.path().join("single.jsonl")).expect("read"),
            b"one\ntwo\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_domain_socket_is_not_a_regular_file() {
        use std::os::unix::net::UnixListener;
        let tmp = tempdir();
        let root = anchor(&tmp);
        let _listener = UnixListener::bind(tmp.path().join("sock")).expect("bind");
        let entry = root.classify_leaf(b"sock").expect("stat").expect("entry");
        assert_eq!(entry.kind, LeafKind::Other);
        assert!(root.open_regular_leaf(b"sock").is_err());
    }

    #[test]
    fn copy_capped_stops_at_the_budget() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        let mut src = root.create_leaf_excl(b"src").expect("src");
        src.write_all(b"0123456789").expect("write src");
        let mut src = root.open_regular_leaf(b"src").expect("open src");
        let mut dst = root.create_leaf_excl(b"dst").expect("dst");
        let error = copy_capped(&mut src.file, &mut dst, 5).expect_err("cap enforced");
        assert!(error.to_string().contains("size limit exceeded"));
        // An exact-fit copy succeeds.
        let mut src = root.open_regular_leaf(b"src").expect("reopen src");
        let mut dst2 = root.create_leaf_excl(b"dst2").expect("dst2");
        let n = copy_capped(&mut src.file, &mut dst2, 10).expect("exact cap");
        assert_eq!(n, 10);
    }

    #[test]
    fn copy_capped_detects_a_file_that_grew_past_the_remaining_budget() {
        // Reader pretends to grow: first chunk 8 bytes, second attempt finds 3
        // more although only 2 remain under the cap.
        struct GrowingReader;
        impl Read for GrowingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                static POS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
                let pos = POS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let chunks: [&[u8]; 2] = [b"01234567", b"abc"];
                let n = (pos as usize).min(chunks.len() - 1);
                let take = buf.len().min(chunks[n].len());
                buf[..take].copy_from_slice(&chunks[n][..take]);
                Ok(take)
            }
        }
        let tmp = tempdir();
        let root = anchor(&tmp);
        let mut dst = root.create_leaf_excl(b"grown").expect("dst");
        let mut reader = GrowingReader;
        let error = copy_capped(&mut reader, &mut dst, 10).expect_err("growth caught");
        assert!(error.to_string().contains("size limit exceeded"));
    }

    #[test]
    fn private_tree_removal_unlinks_symlinks_without_following() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        let temp = root.ensure_subdir(b"tmp-1").expect("temp");
        temp.create_leaf_excl(b"a").expect("a");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("tmp-1/link")).expect("link");
        root.remove_private_tree(b"tmp-1").expect("rmtree");
        assert!(!&tmp.path().join("tmp-1").exists());
        assert!(
            std::path::Path::new("/etc/passwd").exists(),
            "target untouched"
        );
    }

    #[test]
    fn create_subdir_excl_never_adopts_or_replaces_an_existing_name() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        root.ensure_subdir(b"planted").expect("real dir");
        // Exclusive create on an existing name fails (EEXIST); it does not
        // adopt, open, or delete it.
        let error = root.create_subdir_excl(b"planted").expect_err("EEXIST");
        assert!(error.is_already_exists());
        // The existing directory and contents are untouched.
        root.subdir(b"planted")
            .expect("subdir")
            .create_leaf_excl(b"keep")
            .expect("leaf");
        assert!(tmp.path().join("planted/keep").is_file());
    }

    /// Round 7 item 2: a REAL directory reached THROUGH an intermediate symlink
    /// (`<R>/ev -> /outside`, anchoring `<R>/ev/sub`, where `/outside/sub`
    /// exists) must not be pinnable. fstatat(NOFOLLOW) follows intermediate
    /// components, so the final-component check alone returns a real-dir
    /// verdict and canonicalize pins the link target. The only lexical/physical
    /// divergence allowed is a macOS system mount (/tmp, /var).
    #[test]
    fn anchor_refuses_a_real_dir_reached_through_an_intermediate_link() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("sub")).expect("outside/sub");
        symlink(&outside, tmp.path().join("ev")).expect("ev link");
        let via = tmp.path().join("ev/sub");
        let error = DirFd::anchor_existing(&via)
            .expect_err("anchor through an intermediate link is refused");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        let error = DirFd::anchor_or_create(&via)
            .expect_err("anchor-create through an intermediate link is refused");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        // Creating a tail through the link is refused identically.
        let via_new = tmp.path().join("ev/sub/new");
        let error = DirFd::anchor_or_create(&via_new)
            .expect_err("mkdir-tail through an intermediate link is refused");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        assert!(
            !outside.join("sub/new").exists(),
            "nothing is created beyond the link"
        );
        // The real tree itself stays pinnable, and the link remains a walk
        // error below it.
        anchor(&tmp).subdir(b"ev").unwrap_err();
    }

    #[test]
    fn open_or_create_anchor_handles_missing_leaf_components() {
        let tmp = tempdir();
        let target = &tmp.path().join("a/b/c");
        let dir = DirFd::anchor_or_create(target).expect("anchor create");
        dir.create_leaf_excl(b"x").expect("leaf");
        assert!(target.join("x").is_file());
        // Re-open is idempotent.
        DirFd::anchor_or_create(target).expect("re-anchor");
    }

    #[test]
    fn an_exclusive_dir_lock_blocks_a_second_pin_until_released() {
        use std::sync::mpsc;
        use std::time::Duration;
        let tmp = tempdir();
        let path = tmp.path().to_path_buf();
        // A second, INDEPENDENT open description of the same directory: a dup
        // would share the flock, a fresh pin must contend like another process.
        let (tx, rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let dir = DirFd::anchor_existing(&path).expect("anchor");
            let _guard = dir.lock_exclusive().expect("first lock");
            tx.send(()).expect("ready");
            std::thread::sleep(Duration::from_millis(800));
        });
        rx.recv().expect("holder armed");
        let other = anchor(&tmp);
        let started = std::time::Instant::now();
        let _guard2 = other
            .lock_exclusive()
            .expect("second lock blocks then wins");
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "the second pin waited for the holder, not just re-entered the lock"
        );
        holder.join().expect("holder thread");
    }

    #[test]
    fn reject_symlink_descendants_catches_nested_links_and_accepts_a_real_tree() {
        let tmp = tempdir();
        let root = anchor(&tmp);
        // A real tree: dirs and regular files at several levels.
        let a = root.ensure_subdir(b"a").expect("a");
        let b = a.ensure_subdir(b"b").expect("b");
        b.create_leaf_excl(b"f").expect("f");
        a.create_leaf_excl(b"g").expect("g");
        root.reject_symlink_descendants(8)
            .expect("real tree accepted");
        // A nested DIRECTORY symlink must be rejected.
        std::os::unix::fs::symlink("/etc", tmp.path().join("a/b/evil")).expect("nested dir link");
        let error = root
            .reject_symlink_descendants(8)
            .expect_err("nested dir link rejected");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        std::fs::remove_file(tmp.path().join("a/b/evil")).expect("unlink nested dir link");
        // A nested LEAF symlink too (replace the regular leaf with a link).
        std::fs::remove_file(tmp.path().join("a/g")).expect("remove real leaf");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("a/g"))
            .expect("nested leaf link");
        let error = root
            .reject_symlink_descendants(8)
            .expect_err("nested leaf link rejected");
        assert_eq!(error.kind, FdErrorKind::Symlink);
    }
}
