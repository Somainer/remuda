//! How a check's process tree is contained (cb-wake-check r2, r3).
//!
//! Containment is cleanup, not a security boundary: a check runs with the
//! authority of whoever was admitted to run it (main-agent.md §11.2). What it
//! buys is that a finished check leaves nothing behind, and the label of a run
//! says exactly which of these applied:
//!
//! * `cgroup`: a cgroup v2 leaf that the check's outer shell was VERIFIED to
//!   have joined (its `/proc/<pid>/cgroup`). One `cgroup.kill` ends every
//!   process in it, whatever it did; nothing can leave a cgroup by itself.
//! * `subreaper` (Linux without a usable leaf): the outer shell is a child
//!   subreaper, so every orphan below it is reparented to it and found by
//!   walking the tree; see the sweep for its residual limit.
//! * `process-group` (other Unix): only the outer shell's process group is
//!   killed; a check that leaves its group can outlive the report.

use std::path::{Path, PathBuf};

/// Which containment applied to one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Label {
    Cgroup,
    Subreaper,
    ProcessGroup,
}

impl Label {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cgroup => "cgroup",
            Self::Subreaper => "subreaper",
            Self::ProcessGroup => "process-group",
        }
    }

    /// The wire value.
    pub(crate) fn wire(self) -> remuda_protocol::WakeCheckContainment {
        match self {
            Self::Cgroup => remuda_protocol::WakeCheckContainment::Cgroup,
            Self::Subreaper => remuda_protocol::WakeCheckContainment::Subreaper,
            Self::ProcessGroup => remuda_protocol::WakeCheckContainment::ProcessGroup,
        }
    }

    /// The label without a verified cgroup leaf, on this platform.
    pub(crate) fn without_leaf() -> Self {
        if cfg!(target_os = "linux") {
            Self::Subreaper
        } else {
            Self::ProcessGroup
        }
    }
}

/// A cgroup v2 leaf created for one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Leaf {
    /// Its directory.
    pub(crate) dir: PathBuf,
    /// Its path as `/proc/<pid>/cgroup` names it (`/a/b/remuda-wake/x`).
    pub(crate) rel: String,
}

/// A cgroup v2 hierarchy as seen by this process: its mount and the
/// process's own `/proc/self/cgroup`. Parameterised so the decisions can be
/// tested against a fake tree.
#[derive(Debug, Clone)]
pub(crate) struct CgroupFs {
    pub(crate) root: PathBuf,
    pub(crate) self_cgroup: String,
}

/// The directory every check leaf goes under, when the Node's own cgroup has
/// its controllers enabled for its children.
const INTERMEDIATE: &str = "remuda-wake";

fn trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

impl CgroupFs {
    /// The system's: cgroup2 at `/sys/fs/cgroup` (Linux only).
    pub(crate) fn system() -> Option<Self> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        Some(Self {
            root: PathBuf::from("/sys/fs/cgroup"),
            self_cgroup: std::fs::read_to_string("/proc/self/cgroup").ok()?,
        })
    }

    /// The unified-hierarchy path of the process whose `/proc/<pid>/cgroup`
    /// is `text` (its `0::` line).
    pub(crate) fn unified(text: &str) -> Option<&str> {
        text.lines().find_map(|line| line.strip_prefix("0::"))
    }

    /// The directory a check leaf is created under, or `None` when this
    /// hierarchy cannot hold one: cgroup2 is not mounted at the root (no
    /// `cgroup.controllers`, e.g. cgroup v1), the process has no unified path,
    /// or its cgroup is not writable (not delegated).
    ///
    /// The kernel's "no internal processes" rule decides the layout. When the
    /// Node's cgroup enables no controllers for its children
    /// (`cgroup.subtree_control` empty), a leaf goes directly under it. When it
    /// does (a delegated domain parent), a process can only join a cgroup that
    /// distributes nothing further, so the leaf goes under an intermediate
    /// `remuda-wake` directory created with an empty `subtree_control`; one
    /// that already exists and distributes controllers is not used.
    pub(crate) fn leaf_parent(&self) -> Option<PathBuf> {
        if !self.root.join("cgroup.controllers").exists() {
            return None;
        }
        let rel = Self::unified(&self.self_cgroup)?;
        let base = self.root.join(rel.trim_start_matches('/'));
        if !base.is_dir() || !writable(&base) {
            return None;
        }
        let distributes = trimmed(&base.join("cgroup.subtree_control"))
            .is_some_and(|controllers| !controllers.is_empty());
        if !distributes {
            return Some(base);
        }
        let intermediate = base.join(INTERMEDIATE);
        match std::fs::create_dir(&intermediate) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
        let clean = trimmed(&intermediate.join("cgroup.subtree_control"))
            .is_none_or(|controllers| controllers.is_empty());
        clean.then_some(intermediate)
    }

    /// A fresh leaf for one check, or `None` when [`Self::leaf_parent`] has
    /// none or the new directory is not a usable leaf.
    pub(crate) fn create_leaf(&self, name: &str) -> Option<Leaf> {
        let parent = self.leaf_parent()?;
        let dir = parent.join(name);
        std::fs::create_dir(&dir).ok()?;
        if !leaf_ready(&dir) {
            let _ = std::fs::remove_dir(&dir);
            return None;
        }
        let rel = dir.strip_prefix(&self.root).ok()?.to_str()?;
        Some(Leaf {
            rel: format!("/{rel}"),
            dir,
        })
    }
}

/// Whether `dir` can hold a check: the kernel made its `cgroup.procs` and
/// `cgroup.kill` (kernel 5.14+), it distributes no controllers, and it is a
/// plain domain (not `domain invalid` or threaded, which take no processes
/// by `cgroup.procs`).
pub(crate) fn leaf_ready(dir: &Path) -> bool {
    dir.join("cgroup.procs").exists()
        && dir.join("cgroup.kill").exists()
        && trimmed(&dir.join("cgroup.subtree_control")).is_none_or(|text| text.is_empty())
        && trimmed(&dir.join("cgroup.type")).is_none_or(|kind| kind == "domain")
}

fn writable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        nix::unistd::access(dir, nix::unistd::AccessFlags::W_OK).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

/// Keep `leaf` only when the process whose `/proc/<pid>/cgroup` reads
/// `member` is in it: the pre-exec `cgroup.procs` write can fail (EBUSY on a
/// cgroup that distributes controllers, EACCES without delegation, a short
/// write), and the child then runs on outside the leaf. A leaf that was not
/// joined is removed and never killed through.
pub(crate) fn confirm(leaf: Leaf, member: Option<&str>) -> Option<Leaf> {
    let joined = member
        .and_then(CgroupFs::unified)
        .is_some_and(|path| path == leaf.rel);
    if joined {
        Some(leaf)
    } else {
        let _ = std::fs::remove_dir(&leaf.dir);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake cgroup2 mount with the Node's own cgroup at `/svc/node`.
    fn fake(subtree_control: &str) -> (tempfile::TempDir, CgroupFs) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        let base = root.path().join("svc/node");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("cgroup.subtree_control"), subtree_control).unwrap();
        let fs = CgroupFs {
            root: root.path().to_path_buf(),
            self_cgroup: "0::/svc/node\n".into(),
        };
        (root, fs)
    }

    fn kernel_files(dir: &Path, kind: &str) {
        std::fs::create_dir_all(dir).unwrap();
        for file in ["cgroup.procs", "cgroup.kill", "cgroup.subtree_control"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        std::fs::write(dir.join("cgroup.type"), format!("{kind}\n")).unwrap();
    }

    #[test]
    fn a_node_cgroup_without_controllers_takes_its_leaf_directly() {
        let (root, fs) = fake("");
        assert_eq!(fs.leaf_parent(), Some(root.path().join("svc/node")));
    }

    /// The EBUSY domain case: a delegated parent that enables controllers for
    /// its children cannot take processes in a child that distributes them
    /// too, so the leaf goes under an intermediate that distributes nothing.
    #[test]
    fn a_domain_parent_with_controllers_gets_an_intermediate_leaf_domain() {
        let (root, fs) = fake("memory pids\n");
        let parent = fs.leaf_parent().unwrap();
        assert_eq!(parent, root.path().join("svc/node/remuda-wake"));
        assert!(parent.is_dir());
        // An intermediate that distributes controllers itself is not used.
        std::fs::write(parent.join("cgroup.subtree_control"), "memory\n").unwrap();
        assert_eq!(fs.leaf_parent(), None);
    }

    #[test]
    fn no_cgroup2_no_unified_path_or_no_delegation_means_no_leaf() {
        let (root, mut fs) = fake("");
        std::fs::remove_file(root.path().join("cgroup.controllers")).unwrap();
        assert_eq!(fs.leaf_parent(), None, "cgroup2 not mounted (v1)");
        std::fs::write(root.path().join("cgroup.controllers"), "").unwrap();
        fs.self_cgroup = "4:pids:/svc/node\n1:name=systemd:/svc/node\n".into();
        assert_eq!(fs.leaf_parent(), None, "no 0:: line");
        fs.self_cgroup = "0::/svc/elsewhere\n".into();
        assert_eq!(fs.leaf_parent(), None, "own cgroup missing");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs.self_cgroup = "0::/svc/node\n".into();
            let base = root.path().join("svc/node");
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).unwrap();
            let refused = fs.leaf_parent();
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(refused, None, "not delegated (not writable)");
        }
    }

    #[test]
    fn only_a_kernel_made_plain_domain_leaf_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        // A directory the kernel did not populate (a fake tree's mkdir).
        let bare = dir.path().join("bare");
        std::fs::create_dir(&bare).unwrap();
        assert!(!leaf_ready(&bare));
        let leaf = dir.path().join("leaf");
        kernel_files(&leaf, "domain");
        assert!(leaf_ready(&leaf));
        for kind in ["domain invalid", "threaded"] {
            let odd = dir.path().join(kind.replace(' ', "-"));
            kernel_files(&odd, kind);
            assert!(!leaf_ready(&odd), "{kind}");
        }
        let distributing = dir.path().join("distributing");
        kernel_files(&distributing, "domain");
        std::fs::write(distributing.join("cgroup.subtree_control"), "pids\n").unwrap();
        assert!(!leaf_ready(&distributing));
        // In a fake tree create_leaf's own mkdir makes no kernel files: no leaf,
        // and the directory it made is gone again.
        let (root, fs) = fake("");
        assert_eq!(fs.create_leaf("check-1"), None);
        assert!(!root.path().join("svc/node/check-1").exists());
    }

    /// A failed join (EBUSY, EACCES, a short write) leaves the child where
    /// it was: the leaf is dropped, so nothing is killed through it and the
    /// label falls back.
    #[test]
    fn a_leaf_the_child_did_not_join_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let leaf_dir = dir.path().join("svc/node/remuda-wake/check-1");
        kernel_files(&leaf_dir, "domain");
        let leaf = Leaf {
            dir: leaf_dir.clone(),
            rel: "/svc/node/remuda-wake/check-1".into(),
        };
        assert_eq!(
            confirm(leaf.clone(), Some("0::/svc/node/remuda-wake/check-1\n")),
            Some(leaf.clone())
        );
        // The kernel removes its own files with the directory; a fake tree
        // has to empty it for `rmdir` to model that.
        for file in [
            "cgroup.procs",
            "cgroup.kill",
            "cgroup.subtree_control",
            "cgroup.type",
        ] {
            std::fs::remove_file(leaf_dir.join(file)).unwrap();
        }
        for member in [
            Some("0::/svc/node\n"),
            Some("0::/svc/node/remuda-wake\n"),
            None,
        ] {
            let _ = std::fs::create_dir_all(&leaf_dir);
            assert_eq!(confirm(leaf.clone(), member), None, "{member:?}");
            assert!(!leaf_dir.exists(), "the unjoined leaf is removed");
        }
    }
}
