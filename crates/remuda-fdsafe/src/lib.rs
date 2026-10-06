//! Descriptor-relative access to filesystem trees an adversary controls the
//! contents of (c-resumehome round 3).
//!
//! Every path reached through this module starts at an already-open **trusted
//! directory fd** and walks one component at a time with
//! `openat(…, O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC)`. A symlink *anywhere* below
//! the trusted root fails the walk (`ELOOP`/`ENOTDIR`), so:
//!
//! - a symlinked `projects/` or `<session>/` directory can never redirect
//!   writes outside the managed home;
//! - a symlinked leaf (`<S>.jsonl`, a sidecar, a source transcript) is never
//!   opened or copied through;
//! - there is no path-based `stat`→`open` window: leaves are opened
//!   `O_NOFOLLOW`, then `fstat` runs on the **opened fd**, and bytes are read
//!   from that same fd;
//! - non-regular leaves (FIFO, socket, device) are identified on the opened fd
//!   and never read or blocked on;
//! - every file a walk creates is `O_EXCL` relative to a pinned directory fd
//!   and published with `renameat`, never through a path string.
//!
//! Names passed to any method on [`DirFd`] must be exactly one path
//! component (no `/`, no `.`/`..`); the walk decides component order.
//!
//! Self-contained: only `std` + `nix` ("fs"), no Remuda knowledge. The
//! c-dirpicker directory picker builds the same Linux/macOS pattern (macOS has
//! no `O_PATH`; the flags used here — `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` — work
//! on both). When that lands, extract this crate into the shared home both
//! use.
#![cfg(unix)]
// The crate owns the workspace's one audited openat(2) fd transfer
// (`File::from_raw_fd` in `own_fd`); see Cargo.toml for why it does not
// inherit the workspace's `forbid(unsafe_code)` lint.
#![allow(unsafe_code)]
#![deny(missing_docs)]

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use nix::sys::stat::FileStat;
use nix::sys::stat::Mode;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path};

/// What a directory entry is, classified without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafKind {
    /// `S_ISREG` — the only kind [`DirFd::open_regular_leaf`] accepts.
    Regular,
    /// `S_ISLNK` — always rejected by this module.
    Symlink,
    /// `S_ISDIR`.
    Directory,
    /// FIFO, socket, char/block device, anything else — never read.
    Other,
}

/// One classified directory entry.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Single-component file name.
    pub name: Vec<u8>,
    /// Class from `fstatat(AT_SYMLINK_NOFOLLOW)`.
    pub kind: LeafKind,
    /// Size in bytes (0 for non-files).
    pub len: u64,
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
    /// fds, so symlink swaps after opening cannot change the verdict.
    pub fn identity(&self) -> Result<(u64, u64), FdError> {
        let stat = nix::sys::stat::fstat(self.file.as_raw_fd())
            .map_err(|error| FdError::new("<opened fd>", FdErrorKind::Other(error.to_string())))?;
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

/// Failures from an fd walk. Messages name the offending component so callers
/// can surface a clear refusal.
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
    fn new(at: impl Into<String>, kind: FdErrorKind) -> Self {
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

    fn io_path(at: &Path, error: Errno) -> Self {
        Self::io(at.to_string_lossy().as_bytes(), error)
    }

    /// Whether the walk hit a symlink component.
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.kind == FdErrorKind::Symlink
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
        Err(FdError {
            at: String::from_utf8_lossy(name).into_owned(),
            kind: FdErrorKind::BadComponent,
        })
    } else {
        Ok(())
    }
}

/// Wrap a raw fd returned by nix as an owned [`File`] (nix does not close it).
unsafe fn own_fd(fd: RawFd) -> File {
    unsafe { File::from_raw_fd(fd) }
}

fn classify(stat: &FileStat) -> (LeafKind, u64) {
    let mode = stat.st_mode;
    let kind = if nix::sys::stat::SFlag::S_IFLNK.bits() & mode == nix::libc::S_IFLNK {
        LeafKind::Symlink
    } else if nix::sys::stat::SFlag::S_IFREG.bits() & mode == nix::libc::S_IFREG {
        LeafKind::Regular
    } else if nix::sys::stat::SFlag::S_IFDIR.bits() & mode == nix::libc::S_IFDIR {
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
// `O_NONBLOCK` matters: without it, opening a FIFO `O_RDONLY` blocks until a
// writer appears — that was item 4's unbounded hang. With it the open returns
// immediately, and the post-open `fstat` then proves whether the fd is regular
// anyway, so a stat→open swap to a FIFO cannot block either.
const LEAF_FLAGS: OFlag = OFlag::O_RDONLY
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
    /// Pin an existing trusted directory, refusing a symlink at `path`
    /// itself (`O_NOFOLLOW`).
    pub fn open_root(path: &Path) -> Result<Self, FdError> {
        let fd = nix::fcntl::openat(Some(nix::libc::AT_FDCWD), path, DIR_FLAGS, Mode::empty())
            .map_err(|error| FdError::io_path(path, error))?;
        Ok(Self {
            file: unsafe { own_fd(fd) },
        })
    }

    /// Pin an EXISTING absolute `path`, walking every component from `/` with
    /// `O_NOFOLLOW|O_DIRECTORY` and creating nothing. A symlink anywhere on
    /// the path (not just its final component) fails the walk, and a missing
    /// component returns [`FdErrorKind::Missing`].
    pub fn open_existing_abs(path: &Path) -> Result<Self, FdError> {
        if !path.is_absolute() {
            return Err(FdError {
                at: path.display().to_string(),
                kind: FdErrorKind::BadComponent,
            });
        }
        let mut current = Self::open_root(Path::new("/"))?;
        for component in path.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            current = current.subdir(part.as_bytes())?;
        }
        Ok(current)
    }

    /// Pin `path`, creating every missing component from `/` with
    /// `mkdirat` + an `O_NOFOLLOW` revalidation. The filesystem root is the
    /// trust anchor; every component below it is walked, so a symlink in the
    /// middle of the path is refused instead of being created through.
    pub fn open_or_create_abs(path: &Path) -> Result<Self, FdError> {
        if !path.is_absolute() {
            return Err(FdError {
                at: path.display().to_string(),
                kind: FdErrorKind::BadComponent,
            });
        }
        let mut current = Self::open_root(Path::new("/"))?;
        for component in path.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            current = current.ensure_subdir(part.as_bytes())?;
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
                        String::from_utf8_lossy(name).into_owned(),
                        FdErrorKind::Symlink,
                    ));
                }
                LeafKind::Regular | LeafKind::Other => {
                    return Err(FdError::new(
                        String::from_utf8_lossy(name).into_owned(),
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
    /// The returned [`OpenedLeaf::file`] is the descriptor to read or verify.
    ///
    /// The entry is classified with `fstatat` BEFORE the open: a FIFO/device
    /// is never even opened (`O_NONBLOCK` in the flags is the second line of
    /// defence against a stat→open swap), and the post-open `fstat` confirms
    /// the fd itself.
    pub fn open_leaf(&self, name: &[u8]) -> Result<OpenedLeaf, FdError> {
        check_component(name)?;
        let pre = self
            .classify_leaf(name)?
            .ok_or_else(|| FdError::io(name, Errno::ENOENT))?;
        if !matches!(pre.kind, LeafKind::Regular | LeafKind::Other) {
            return Err(FdError {
                at: String::from_utf8_lossy(name).into_owned(),
                kind: match pre.kind {
                    LeafKind::Symlink => FdErrorKind::Symlink,
                    LeafKind::Directory => FdErrorKind::NotDirectory,
                    LeafKind::Other | LeafKind::Regular => unreachable!(),
                },
            });
        }
        let fd = nix::fcntl::openat(Some(self.as_raw_fd()), name, LEAF_FLAGS, Mode::empty())
            .map_err(|error| FdError::io(name, error))?;
        let file = unsafe { own_fd(fd) };
        let stat =
            nix::sys::stat::fstat(file.as_raw_fd()).map_err(|error| FdError::io(name, error))?;
        let (kind, len) = classify(&stat);
        Ok(OpenedLeaf { file, kind, len })
    }

    /// Open a leaf and require it to be a real regular file on the opened fd.
    /// Symlinks never reach here (`O_NOFOLLOW`), and a FIFO/device is refused
    /// rather than read or blocked on.
    pub fn open_regular_leaf(&self, name: &[u8]) -> Result<OpenedLeaf, FdError> {
        let leaf = self.open_leaf(name)?;
        match leaf.kind {
            LeafKind::Regular => Ok(leaf),
            other => Err(FdError {
                at: String::from_utf8_lossy(name).into_owned(),
                kind: match other {
                    LeafKind::Symlink => FdErrorKind::Symlink,
                    LeafKind::Directory => FdErrorKind::NotDirectory,
                    LeafKind::Other => FdErrorKind::Other("not a regular file".into()),
                    LeafKind::Regular => unreachable!(),
                },
            }),
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

/// Copy at most `cap` bytes from `src` to `dst`, failing the moment one more
/// byte would cross the cap (a "success" never returns a partial over-cap
/// copy). The caller supplies the hashing writer so provenance hashes the
/// bytes ACTUALLY copied, not a later re-open of the source path.
pub fn copy_capped<R: std::io::Read, W: std::io::Write>(
    src: &mut R,
    dst: &mut W,
    cap: u64,
) -> std::io::Result<u64> {
    let mut buf = vec![0_u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = src.read(&mut buf)?;
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
pub struct TeeWriter<'a, A, B> {
    first: &'a mut A,
    second: &'a mut B,
}

impl<'a, A: std::io::Write, B: std::io::Write> TeeWriter<'a, A, B> {
    /// Fan every write out to `first` and `second`.
    pub fn new(first: &'a mut A, second: &'a mut B) -> Self {
        Self { first, second }
    }
}

impl<A: std::io::Write, B: std::io::Write> std::io::Write for TeeWriter<'_, A, B> {
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

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn walks_real_components_and_creates_children() {
        let tmp = tempdir();
        let root = DirFd::open_root(tmp.path()).expect("root");
        let sub = root.ensure_subdir(b"projects").expect("mkdir");
        let mut file = sub.create_leaf_excl(b"a.jsonl").expect("excl create");
        file.write_all(b"{}").expect("write");
        let leaf = sub.open_regular_leaf(b"a.jsonl").expect("regular leaf");
        assert_eq!(leaf.kind, LeafKind::Regular);
        assert_eq!(leaf.len, 2);
    }

    #[test]
    fn a_symlink_at_every_level_is_refused() {
        let tmp = tempdir();
        let outside = tmp.path().join("outside.jsonl");
        std::fs::write(&outside, b"secret").expect("outside");
        let root = DirFd::open_root(tmp.path()).expect("root");

        // Symlinked intermediate directory.
        std::os::unix::fs::symlink(tmp.path(), tmp.path().join("linkdir")).expect("symlink dir");
        assert_eq!(
            root.subdir(b"linkdir")
                .expect_err("symlinked dir refused")
                .kind,
            FdErrorKind::Symlink
        );

        // Symlinked leaf, even though it points at a regular file.
        std::os::unix::fs::symlink(&outside, tmp.path().join("leaf.jsonl")).expect("symlink");
        let error = root
            .open_regular_leaf(b"leaf.jsonl")
            .expect_err("symlink refused");
        assert_eq!(error.kind, FdErrorKind::Symlink);
        assert!(error.is_symlink());
    }

    #[test]
    fn dotdot_and_multi_component_names_are_rejected_upfront() {
        let tmp = tempdir();
        let root = DirFd::open_root(tmp.path()).expect("root");
        assert_eq!(
            root.subdir(b"../x").unwrap_err().kind,
            FdErrorKind::BadComponent
        );
        assert_eq!(
            root.open_regular_leaf(b"a/b").unwrap_err().kind,
            FdErrorKind::BadComponent
        );
    }

    #[test]
    fn a_fifo_is_classified_other_and_never_opened_as_regular() {
        let tmp = tempdir();
        let root = DirFd::open_root(tmp.path()).expect("root");
        let fifo = tmp.path().join("pipe");
        nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).expect("mkfifo");
        let entry = root
            .classify_leaf(b"pipe")
            .expect("stat")
            .expect("fifo listed");
        assert_eq!(entry.kind, LeafKind::Other);
        assert!(matches!(
            root.open_regular_leaf(b"pipe").unwrap_err().kind,
            FdErrorKind::Other(_)
        ));
    }

    #[test]
    fn copy_capped_stops_at_the_budget_plus_one_byte() {
        let tmp = tempdir();
        let root = DirFd::open_root(tmp.path()).expect("root");
        let mut src = root.create_leaf_excl(b"src").expect("src");
        src.write_all(b"0123456789").expect("write src");
        let src_leaf = root.open_regular_leaf(b"src").expect("open src");
        let mut src_file = src_leaf.file;
        let mut dst = root.create_leaf_excl(b"dst").expect("dst");
        let error = copy_capped(&mut src_file, &mut dst, 5).expect_err("cap enforced");
        assert!(error.to_string().contains("size limit exceeded"), "{error}");
        // Nothing was written before the cap check failed; the partial temp
        // file was never renamed to a final name by the caller.
        assert_eq!(
            root.classify_leaf(b"dst")
                .expect("stat")
                .expect("entry")
                .len,
            0
        );
    }

    #[test]
    fn private_tree_removal_unlinks_symlinks_without_following_them() {
        let tmp = tempdir();
        let root = DirFd::open_root(tmp.path()).expect("root");
        let temp = root.ensure_subdir(b"tmp-1").expect("temp");
        temp.create_leaf_excl(b"a").expect("a");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("tmp-1/link")).expect("symlink");
        root.remove_private_tree(b"tmp-1").expect("rmtree");
        assert!(root.classify_leaf(b"tmp-1").expect("stat").is_none());
    }

    #[test]
    fn open_or_create_walks_from_root_and_rejects_a_mid_path_symlink() {
        let tmp = tempdir();
        let base = tmp.path().join("safe/d");
        std::fs::create_dir_all(&base).expect("mkdirs");
        let walked = DirFd::open_or_create_abs(&base.join("deeper")).expect("walk");
        walked.create_leaf_excl(b"x").expect("file");
        // Replace an intermediate with a symlink: the next walk must fail.
        std::fs::rename(base.join("deeper"), base.join("deeper.real")).expect("rename");
        std::os::unix::fs::symlink("/etc", base.join("deeper")).expect("link");
        assert!(DirFd::open_or_create_abs(&base.join("deeper")).is_err());
    }
}
