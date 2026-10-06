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
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FileStat, Mode};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path};

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
    /// The `(st_dev, st_ino)` pair of the opened file description. Two leaves
    /// are the same on-disk file iff their pairs match — read from the opened
    /// fds, so a symlink swap after opening cannot change the verdict.
    pub fn identity(&self) -> Result<(u64, u64), FdError> {
        let stat = fstat_fd(self.file.as_raw_fd())?;
        Ok((dev_of(&stat), ino_of(&stat)))
    }
}

/// An owned directory fd pinned to one trusted directory.
#[derive(Debug)]
pub struct DirFd {
    file: File,
}

impl AsRawFd for DirFd {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
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
            FdErrorKind::Other(ref message) => write!(f, "{:?}: {message}", self.at),
        }
    }
}

impl std::error::Error for FdError {}

impl From<FdError> for std::io::Error {
    fn from(error: FdError) -> Self {
        let kind = match error.kind {
            FdErrorKind::Symlink | FdErrorKind::NotDirectory => {
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
    /// Pin a TRUSTED ANCHOR directory.
    ///
    /// Unlike every other entry in the module, symlinks IN the anchor path
    /// itself are resolved once with `realpath`/`canonicalize`: the anchor is
    /// configured by trusted setup (a managed native home, a test allocation
    /// base), and on macOS system paths like `/tmp → /private/tmp` and
    /// `/var → /private/var` are mount symlinks an O_NOFOLLOW walk from `/`
    /// cannot cross. The canonical path is then opened with
    /// `O_NOFOLLOW|O_DIRECTORY`; everything BELOW it is walked link-free.
    pub fn anchor_existing(path: &Path) -> Result<Self, FdError> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| FdError::new(path.display().to_string(), errno_kind(error)))?;
        let fd = nix::fcntl::openat(
            Some(nix::libc::AT_FDCWD),
            &canonical,
            DIR_FLAGS,
            Mode::empty(),
        )
        .map_err(|error| match error {
            Errno::ELOOP => FdError::new(canonical.display().to_string(), FdErrorKind::Symlink),
            Errno::ENOTDIR => {
                FdError::new(canonical.display().to_string(), FdErrorKind::NotDirectory)
            }
            Errno::ENOENT => FdError::new(canonical.display().to_string(), FdErrorKind::Missing),
            other => FdError::new(
                canonical.display().to_string(),
                FdErrorKind::Other(other.to_string()),
            ),
        })?;
        Ok(Self {
            file: unsafe { own_fd(fd) },
        })
    }

    /// Pin an anchor, creating it 0700 if absent.
    ///
    /// The deepest EXISTING ancestor is realpath'd once (crossing its system
    /// mount symlinks); every component below that ancestor is created
    /// `mkdirat` + `O_NOFOLLOW`, so a symlink under the anchor cannot be
    /// created through even when the anchor path did not previously exist.
    pub fn anchor_or_create(path: &Path) -> Result<Self, FdError> {
        if !path.is_absolute() {
            return Err(FdError::new(
                path.display().to_string(),
                FdErrorKind::BadComponent,
            ));
        }
        // Find the deepest existing ancestor.
        let mut existing = path.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if existing.is_dir() {
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
            let parent = match existing.parent() {
                Some(parent) if parent != existing => parent.to_path_buf(),
                _ => {
                    return Err(FdError::new(
                        path.display().to_string(),
                        FdErrorKind::BadComponent,
                    ));
                }
            };
            existing = parent;
        }
        let mut current = Self::anchor_existing(&existing)?;
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
    /// open; the post-open `fstat` requires a regular file on the fd. Absence
    /// is [`FdErrorKind::Missing`], a symlink is [`FdErrorKind::Symlink`].
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

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::path::PathBuf::from(format!(
            "/tmp/remuda-fdsafe-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn anchor(tmp: &std::path::Path) -> DirFd {
        DirFd::anchor_existing(tmp).expect("anchor")
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
        let project = &tmp.join("real-dir");
        std::fs::create_dir_all(project).expect("mkdir");
        use std::os::unix::fs::symlink;
        symlink(project, tmp.join("linkdir")).expect("symlink dir");
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
        let fifo = &tmp.join("pipe");
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
            tmp.join("hose").as_os_str(),
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

    #[cfg(unix)]
    #[test]
    fn a_unix_domain_socket_is_not_a_regular_file() {
        use std::os::unix::net::UnixListener;
        let tmp = tempdir();
        let root = anchor(&tmp);
        let _listener = UnixListener::bind(tmp.join("sock")).expect("bind");
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
        std::os::unix::fs::symlink("/etc/passwd", tmp.join("tmp-1/link")).expect("link");
        root.remove_private_tree(b"tmp-1").expect("rmtree");
        assert!(!&tmp.join("tmp-1").exists());
        assert!(
            std::path::Path::new("/etc/passwd").exists(),
            "target untouched"
        );
    }

    #[test]
    fn open_or_create_anchor_handles_missing_leaf_components() {
        let tmp = tempdir();
        let target = &tmp.join("a/b/c");
        let dir = DirFd::anchor_or_create(target).expect("anchor create");
        dir.create_leaf_excl(b"x").expect("leaf");
        assert!(target.join("x").is_file());
        // Re-open is idempotent.
        DirFd::anchor_or_create(target).expect("re-anchor");
    }
}
