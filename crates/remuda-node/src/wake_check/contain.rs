//! How a check's process tree is contained (cb-wake-check r2, r3, r4).
//!
//! Containment is cleanup, not a security boundary: a check runs with the
//! authority of whoever was admitted to run it (main-agent.md §11.2). What it
//! buys is that a finished check leaves nothing behind, and the label of a run
//! says exactly which of these applied:
//!
//! * `cgroup`: the check's outer shell was VERIFIED (its `/proc/<pid>/cgroup`)
//!   to be in a leaf of a cgroup v2 domain made for this check alone, under
//!   the Node's delegated cgroup (`<node>/remuda-wake-<id>/check`).
//!   `cgroup.kill` on the leaf and on that per-check domain ends every process
//!   in either, whatever `setsid`, a double fork or a cleared environment did,
//!   and also one that moved itself from the leaf up into its domain.
//!   **Residual:** a process may write its own pid into the `cgroup.procs` of
//!   any cgroup its user can write, the Node's delegated cgroup or another
//!   writable ancestor included; one that does so (and also detaches, clears
//!   its marker and kills its supervisor) is out of every reach and can
//!   outlive the report. Under the ruling above that is not an escalation of
//!   authority.
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
    /// Its directory, inside [`Leaf::domain`].
    pub(crate) dir: PathBuf,
    /// The domain made for this check alone, the leaf's parent.
    pub(crate) domain: PathBuf,
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

/// The prefix of the per-check domain directory.
const DOMAIN_PREFIX: &str = "remuda-wake-";
/// The leaf's name inside its domain.
const LEAF: &str = "check";

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

    /// The directory a check's domain is created under (the Node's own
    /// cgroup), or `None` when this hierarchy cannot hold one: cgroup2 is not
    /// mounted at the root (no `cgroup.controllers`, e.g. cgroup v1), the
    /// process has no unified path, or its cgroup is not writable (not
    /// delegated).
    pub(crate) fn leaf_parent(&self) -> Option<PathBuf> {
        if !self.root.join("cgroup.controllers").exists() {
            return None;
        }
        let rel = Self::unified(&self.self_cgroup)?;
        let base = self.root.join(rel.trim_start_matches('/'));
        (base.is_dir() && writable(&base)).then_some(base)
    }

    /// Where one check's domain and leaf go: `<base>/remuda-wake-<name>` and
    /// its `check` leaf.
    pub(crate) fn layout(&self, name: &str) -> Option<(PathBuf, PathBuf)> {
        let domain = self.leaf_parent()?.join(format!("{DOMAIN_PREFIX}{name}"));
        let leaf = domain.join(LEAF);
        Some((domain, leaf))
    }

    /// A fresh per-check domain and its leaf, or `None` when the hierarchy has
    /// no place for them or the kernel did not make them usable.
    ///
    /// The kernel's "no internal processes" rule decides the layout. Whatever
    /// the Node's cgroup distributes, a fresh domain distributes nothing (its
    /// `cgroup.subtree_control` starts empty), so its `check` leaf can take
    /// processes; and the domain belongs to this check alone, so killing it
    /// also ends a process that left the leaf for its domain.
    pub(crate) fn create_leaf(&self, name: &str) -> Option<Leaf> {
        let (domain, dir) = self.layout(name)?;
        std::fs::create_dir(&domain).ok()?;
        let domain_ready = domain.join("cgroup.kill").exists()
            && trimmed(&domain.join("cgroup.subtree_control")).is_none_or(|text| text.is_empty());
        if !domain_ready || std::fs::create_dir(&dir).is_err() || !leaf_ready(&dir) {
            let _ = std::fs::remove_dir(&dir);
            let _ = std::fs::remove_dir(&domain);
            return None;
        }
        let rel = dir.strip_prefix(&self.root).ok()?.to_str()?;
        Some(Leaf {
            rel: format!("/{rel}"),
            dir,
            domain,
        })
    }
}

impl Leaf {
    /// `cgroup.kill` on the leaf and on its per-check domain: every process in
    /// either ends.
    pub(crate) fn kill(&self) {
        let _ = std::fs::write(self.dir.join("cgroup.kill"), "1");
        let _ = std::fs::write(self.domain.join("cgroup.kill"), "1");
    }

    /// The pids in the leaf and in its domain.
    pub(crate) fn members(&self) -> Vec<i32> {
        [&self.dir, &self.domain]
            .into_iter()
            .filter_map(|dir| std::fs::read_to_string(dir.join("cgroup.procs")).ok())
            .flat_map(|procs| {
                procs
                    .lines()
                    .filter_map(|pid| pid.trim().parse().ok())
                    .collect::<Vec<i32>>()
            })
            .collect()
    }

    /// Remove the leaf, then its domain; `true` once both are gone.
    pub(crate) fn remove(&self) -> bool {
        let _ = std::fs::remove_dir(&self.dir);
        let _ = std::fs::remove_dir(&self.domain);
        !self.dir.exists() && !self.domain.exists()
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
        leaf.remove();
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

    fn leaf_in(root: &Path, name: &str) -> Leaf {
        let domain = root.join(format!("svc/node/remuda-wake-{name}"));
        let dir = domain.join("check");
        kernel_files(&domain, "domain");
        kernel_files(&dir, "domain");
        Leaf {
            rel: format!("/svc/node/remuda-wake-{name}/check"),
            dir,
            domain,
        }
    }

    /// r4: each check gets a domain of its own, whatever the Node's cgroup
    /// distributes (the EBUSY domain case included: a fresh domain
    /// distributes nothing, so its leaf takes processes).
    #[test]
    fn each_check_gets_its_own_domain_and_leaf() {
        for subtree_control in ["", "memory pids\n"] {
            let (root, fs) = fake(subtree_control);
            let base = root.path().join("svc/node");
            assert_eq!(fs.leaf_parent(), Some(base.clone()));
            let (domain, leaf) = fs.layout("a").unwrap();
            assert_eq!(domain, base.join("remuda-wake-a"));
            assert_eq!(leaf, base.join("remuda-wake-a/check"));
            let (other, _) = fs.layout("b").unwrap();
            assert_ne!(domain, other, "the domain is per check");
        }
    }

    /// r4: a process that moved itself from the leaf up into its per-check
    /// domain is still killed: the kill goes to both, and both are walked.
    #[test]
    fn the_kill_reaches_the_leaf_and_its_per_check_domain() {
        let root = tempfile::tempdir().unwrap();
        let leaf = leaf_in(root.path(), "a");
        let other = leaf_in(root.path(), "b");
        std::fs::write(leaf.dir.join("cgroup.procs"), "101\n").unwrap();
        // 102 left the leaf for its domain.
        std::fs::write(leaf.domain.join("cgroup.procs"), "102\n").unwrap();
        std::fs::write(other.dir.join("cgroup.procs"), "201\n").unwrap();
        assert_eq!(leaf.members(), [101, 102]);
        leaf.kill();
        for dir in [&leaf.dir, &leaf.domain] {
            assert_eq!(
                std::fs::read_to_string(dir.join("cgroup.kill")).unwrap(),
                "1"
            );
        }
        for dir in [&other.dir, &other.domain] {
            assert_eq!(
                std::fs::read_to_string(dir.join("cgroup.kill")).unwrap(),
                "",
                "another check's domain is untouched"
            );
        }
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
        // and the directories it made are gone again.
        let (root, fs) = fake("");
        assert_eq!(fs.create_leaf("1"), None);
        assert!(!root.path().join("svc/node/remuda-wake-1").exists());
    }

    /// A failed join (EBUSY, EACCES, a short write) leaves the child where
    /// it was: the leaf and its domain are dropped, so nothing is killed
    /// through them and the label falls back.
    #[test]
    fn a_leaf_the_child_did_not_join_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = leaf_in(dir.path(), "1");
        assert_eq!(
            confirm(leaf.clone(), Some("0::/svc/node/remuda-wake-1/check\n")),
            Some(leaf.clone())
        );
        // The kernel removes its own files with the directory; a fake tree
        // has to empty them for `rmdir` to model that.
        let empty = |dir: &Path| {
            for file in [
                "cgroup.procs",
                "cgroup.kill",
                "cgroup.subtree_control",
                "cgroup.type",
            ] {
                let _ = std::fs::remove_file(dir.join(file));
            }
        };
        for member in [
            Some("0::/svc/node\n"),
            Some("0::/svc/node/remuda-wake-1\n"),
            None,
        ] {
            let _ = std::fs::create_dir_all(&leaf.dir);
            empty(&leaf.dir);
            empty(&leaf.domain);
            assert_eq!(confirm(leaf.clone(), member), None, "{member:?}");
            assert!(
                !leaf.dir.exists() && !leaf.domain.exists(),
                "both are removed"
            );
        }
    }
}
