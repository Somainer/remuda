//! The embedded `remuda` skill and the seat's capability section (ma-skill,
//! D-060 item 5, main-agent.md §3.4.1).
//!
//! The Hub stamps `skillContext` — the launch facts: grants held, herdr on the
//! host, the project's D-051 switch, whether an inbox exists — on every agent
//! create and resume forwarded to a Node that advertises it. From those facts
//! the materializer does two things:
//!
//! * **The skill tree.** The static skill (`skills/remuda`, embedded here and
//!   byte-identical to the checkout, `tests/remuda_skill.rs`) goes into
//!   `<native_home>/skills/remuda` for the launches whose harness reads that
//!   directory: kind `claude` on the carriers whose argv the materializer
//!   builds (`claude_argv`, and `materialize_shell_pty_agent`), which pin
//!   `CLAUDE_CONFIG_DIR` to the native home; and a `generic-pty` Claude pane
//!   whose home holds login material, the one case where that driver pins the
//!   home too (`generic_pty::pane_home_env`). It follows the computer-use
//!   rules: never the inherited operator home (D-045), and file lifetime by
//!   [`native_home_is_shared`]. It also never overwrites a tree it did not
//!   write, and never follows a symlink at `skills` or `skills/remuda`: a
//!   marker-less directory holding files, and any symlink, is user-owned and
//!   left alone; only an empty real directory is taken over (§3.4.1).
//! * **The capability section.** Rendered by [`render_section`]. It goes into
//!   the installed `SKILL.md` only when the tree is this seat's alone (a
//!   per-instance home); a shared tree is the same bytes for every seat on it,
//!   so there the section — like every launch without a tree — rides the
//!   launch's own text channel, the main-agent.md §12.3 mechanism of its path:
//!   the materializer's own system-prompt pair on the Claude argv paths, the
//!   first-input data block everywhere else.
//!
//! Kinds `terminal` and `generic` get nothing, and neither does a launch
//! without `skillContext` (a Hub or a caller that does not stamp it).

use crate::binary::hash_bytes;
use crate::error::{DriverError, DriverResult};
use crate::launch::skills::native_home_is_shared;
use crate::recipe::{FileLifetime, FileRole, MaterializedFile};
pub use remuda_protocol::hubnode::SkillContext;
use remuda_protocol::hubnode::{SKILL_CONTEXT_CLOSE, SKILL_CONTEXT_OPEN};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Embedded skill directory name (its frontmatter `name`).
pub const SKILL_DIR: &str = "remuda";

/// File Remuda writes beside the skill it installed. A `skills/remuda` tree
/// without it was put there by someone else and is never overwritten.
pub const MANAGED_MARKER: &str = ".remuda-managed";

const MARKER_TEXT: &[u8] = b"Written by Remuda at launch from the skill embedded in the \
remuda binary. Remuda rewrites this tree on every launch that uses this home; \
delete this file to keep your own copy instead.\n";

struct EmbeddedFile {
    /// Path relative to the skill root.
    rel: &'static str,
    bytes: &'static [u8],
}

/// The shipped skill, embedded in the binary. Every shipped file under
/// `skills/remuda/` must be listed here; the digest-equality test fails when
/// the on-disk set and this list disagree. The section sources and the
/// maintainers' README are not shipped ([`SOURCE_ONLY`]).
const EMBEDDED_SKILL: &[EmbeddedFile] = &[EmbeddedFile {
    rel: "SKILL.md",
    bytes: include_str!("../../../../skills/remuda/SKILL.md").as_bytes(),
}];

/// Paths under `skills/remuda/` that are sources for `SKILL.md`, not part of
/// the skill a seat reads (`scripts/gen-skill.sh` assembles them). A trailing
/// `/` names a directory.
pub const SOURCE_ONLY: &[&str] = &["README.md", "sections/"];

/// Iterate the embedded file set (path relative to the skill root, bytes).
pub fn embedded_skill_files() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    EMBEDDED_SKILL.iter().map(|file| (file.rel, file.bytes))
}

/// Why a launch got no skill tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TreeSkip {
    /// The harness or carrier reads no skills directory Remuda can write: every
    /// non-Claude harness, and kind `claude` on `generic-pty` whose home holds
    /// no login (the pane then runs with the operator's own home, so a tree
    /// written here would have no reader).
    NoSkillReader,
    /// The native home is the inherited operator home, which Remuda never
    /// writes (D-045).
    OperatorHome,
    /// The tree is the user's: a symlink at `skills` or at `skills/remuda`
    /// (never followed), or a real `skills/remuda` holding files without
    /// Remuda's marker (kept byte for byte). An empty real directory is not
    /// this case; the launch takes it over.
    UserOwned,
}

/// Where the embedded tree went for this launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum SkillTree {
    /// Written into `root`.
    Installed {
        /// `<native_home>/skills/remuda`.
        root: String,
        /// The home is shared across instances (it outlives this launch).
        shared: bool,
    },
    /// Not written.
    Skipped {
        /// Why.
        reason: TreeSkip,
    },
}

/// How the capability section rides this launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mechanism", rename_all = "kebab-case")]
pub enum SectionDelivery {
    /// Appended to the installed `SKILL.md`.
    Tree,
    /// Inside the materializer's own `--append-system-prompt` pair.
    Flag {
        /// The framed section, exactly as it is joined into the pair.
        text: String,
    },
    /// Framed data the driver delivers ahead of the first input.
    DataBlock {
        /// The framed section, exactly as it precedes the first input.
        text: String,
    },
}

/// What the launch did with `skillContext`, recorded on the recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeSkill {
    /// The facts the section was rendered from.
    pub context: SkillContext,
    /// Where the embedded tree went.
    pub tree: SkillTree,
    /// How the section rides the launch. Absent only on a data-block path's
    /// native resume: the block the conversation started with is still in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<SectionDelivery>,
}

impl RecipeSkill {
    /// The framed section inside the materializer's own pair, if any.
    #[must_use]
    pub fn flag_text(&self) -> Option<&str> {
        match &self.section {
            Some(SectionDelivery::Flag { text }) => Some(text),
            _ => None,
        }
    }

    /// The framed section the driver puts ahead of its first input, if any.
    #[must_use]
    pub fn data_block(&self) -> Option<&str> {
        match &self.section {
            Some(SectionDelivery::DataBlock { text }) => Some(text),
            _ => None,
        }
    }
}

/// Inputs for [`plan`].
pub(crate) struct SkillRequest<'a> {
    pub context: Option<&'a SkillContext>,
    pub kind: remuda_protocol::AgentKind,
    /// The materializer builds this launch's Claude argv itself; the section
    /// rides its own `--append-system-prompt` pair.
    pub claude_argv: bool,
    /// The launch is pinned to `native_home` as the harness's config
    /// directory, so `<native_home>/skills` is read: true on the Claude argv
    /// paths (which pin `CLAUDE_CONFIG_DIR`), and for a `generic-pty` Claude
    /// pane whose home holds login material (the pane is pinned only then,
    /// `generic_pty::pane_home_env`).
    pub reads_skills: bool,
    /// A native resume: the conversation already holds an earlier data block.
    pub resume: bool,
    pub launch_dir: &'a Path,
    pub native_home: &'a Path,
    /// Whether `native_home` is Remuda-managed (false: the inherited operator
    /// home).
    pub native_home_managed: bool,
}

/// Everything [`plan`] decided. Nothing is written until [`SkillPlan::write`]
/// runs, so a launch the materializer refuses later leaves no tree behind.
pub(crate) struct SkillPlan {
    pub recipe: RecipeSkill,
    write: Option<TreeWrite>,
}

struct TreeWrite {
    root: PathBuf,
    shared: bool,
    /// The section appended to `SKILL.md` (per-instance homes only).
    appended: Option<String>,
}

impl SkillPlan {
    /// Write the planned tree, if any, and return the files written.
    pub(crate) fn write(&self) -> DriverResult<Vec<MaterializedFile>> {
        let mut files = Vec::new();
        if let Some(tree) = &self.write {
            write_tree(
                &tree.root,
                tree.shared,
                tree.appended.as_deref(),
                &mut files,
            )?;
        }
        Ok(files)
    }
}

/// Decide the tree and the section for one launch. `None` when the launch
/// carries no `skillContext` or is not an agent kind.
pub(crate) fn plan(request: &SkillRequest<'_>) -> Option<SkillPlan> {
    let context = request.context?;
    if !matches!(
        request.kind,
        remuda_protocol::AgentKind::Claude
            | remuda_protocol::AgentKind::Codex
            | remuda_protocol::AgentKind::Grok
            | remuda_protocol::AgentKind::Agy
    ) {
        return None;
    }
    let section = render_section(context);
    let mut write = None;
    let claude = request.kind == remuda_protocol::AgentKind::Claude;
    let tree = if !(request.reads_skills && claude) {
        SkillTree::Skipped {
            reason: TreeSkip::NoSkillReader,
        }
    } else if !request.native_home_managed {
        SkillTree::Skipped {
            reason: TreeSkip::OperatorHome,
        }
    } else {
        let root = request.native_home.join("skills").join(SKILL_DIR);
        let parent_linked = root.parent().is_some_and(is_symlink);
        if parent_linked || user_owned(&root) {
            SkillTree::Skipped {
                reason: TreeSkip::UserOwned,
            }
        } else {
            let shared = native_home_is_shared(request.launch_dir, request.native_home);
            // The section joins the tree only when the tree is this seat's
            // alone; a shared tree holds the same bytes for every seat on it.
            let installed = SkillTree::Installed {
                root: root.to_string_lossy().into_owned(),
                shared,
            };
            write = Some(TreeWrite {
                root,
                shared,
                appended: (!shared).then(|| section.clone()),
            });
            installed
        }
    };
    let section = match (&tree, request.claude_argv) {
        (SkillTree::Installed { shared: false, .. }, _) => Some(SectionDelivery::Tree),
        // The Claude argv paths re-emit their pair on every process start, a
        // native resume included (main-agent.md §12.3).
        (_, true) => Some(SectionDelivery::Flag {
            text: framed(&section),
        }),
        // A data block is part of the conversation; a resume carries it over.
        (_, false) if request.resume => None,
        (_, false) => Some(SectionDelivery::DataBlock {
            text: framed(&section),
        }),
    };
    Some(SkillPlan {
        recipe: RecipeSkill {
            context: context.clone(),
            tree,
            section,
        },
        write,
    })
}

/// The section framed as data for the system-prompt pair or the data block.
fn framed(section: &str) -> String {
    let body = section.replace(SKILL_CONTEXT_CLOSE, "</remuda-capabilities\u{200b}>");
    format!(
        "{SKILL_CONTEXT_OPEN}>\n{}\n{SKILL_CONTEXT_CLOSE}",
        body.trim_end()
    )
}

/// A `skills/remuda` Remuda must not write: a symlink, a tree with Remuda's
/// marker absent but files in it, or a path that is not a directory at all.
///
/// A symlink is never followed — not even to an empty directory — because its
/// target is a directory Remuda did not create, and writing the marker and the
/// tree through it would put them outside the home. A real directory holding
/// files without the marker is the user's and stays byte for byte. An empty
/// real directory is taken over: it holds no bytes to keep, and it is what a
/// launch's own cleanup leaves when it shreds a per-instance tree, so refusing
/// it would leave the next launch in that home without a tree.
fn user_owned(root: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(root) else {
        // Does not exist: the launch may create it.
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    if !meta.is_dir() {
        return true;
    }
    // One scan, not "is the marker there, then is anything there": the two
    // separate checks have a window in which a sibling launch creates its
    // marker in between, so this launch read a marker-less non-empty tree and
    // skipped. Every writer writes the marker before any other file, so a
    // single listing that holds a file must also hold the marker.
    let Ok(entries) = fs::read_dir(root) else {
        return true;
    };
    let mut any = false;
    for entry in entries.flatten() {
        if entry.file_name() == MANAGED_MARKER {
            return false;
        }
        any = true;
    }
    any
}

/// Whether `path` is a symbolic link (never followed).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Refuse to write when `skills` or `skills/remuda` is a link, or the tree is
/// the user's. Called before anything is created or written, and again right
/// before the first write.
fn refuse_user_owned(skills_root: &Path, root: &Path) -> DriverResult<()> {
    if is_symlink(skills_root) || user_owned(root) {
        return Err(DriverError::SettingsIsolationUnavailable(format!(
            "refusing to write the remuda skill: {} is a symlink or a tree Remuda did not \
             write; the launch runs without a skill tree",
            root.display()
        )));
    }
    Ok(())
}

/// Write the embedded tree (and, for a per-instance home, the section appended
/// to `SKILL.md`) owner-only. Each file is written to a temporary name and
/// renamed, so a seat sharing the home never reads a half-written file.
fn write_tree(
    root: &Path,
    shared: bool,
    appended: Option<&str>,
    files: &mut Vec<MaterializedFile>,
) -> DriverResult<()> {
    let skills_root = root.parent().ok_or_else(|| {
        DriverError::InvalidLaunchSpec("the skill root has no parent directory".into())
    })?;
    // Never follow a link, and not even to create a directory through one:
    // `create_dir_all` would lay a directory down inside the link's target,
    // outside the home.
    refuse_user_owned(skills_root, root)?;
    // Only paths this launch creates get a mode: `skills` may be the user's
    // own directory, and Remuda never changes permissions it did not set.
    ensure_dir(skills_root)?;
    ensure_dir(root)?;
    let lifetime = if shared {
        FileLifetime::NativeStore
    } else {
        FileLifetime::Launch
    };
    // The decision above ran before the launch dir and any overlay were
    // written; re-check now, immediately before the first write, so a link
    // planted in between cannot be won. Nothing has been written yet.
    refuse_user_owned(skills_root, root)?;
    // The marker goes first and directly: the ownership check reads "any
    // entry at all without the marker" as the user's tree, so a launch that
    // ran between a temporary marker file and its rename would see a
    // markerless, non-empty directory and skip the tree. The bytes are fixed
    // text, so a concurrent reader of the file has nothing to lose.
    let marker = root.join(MANAGED_MARKER);
    fs::write(&marker, MARKER_TEXT)?;
    set_mode(&marker, 0o600)?;
    files.push(MaterializedFile {
        path: marker.to_string_lossy().into_owned(),
        role: FileRole::RemudaSkill,
        mode: "0600".into(),
        content_digest: hash_bytes(MARKER_TEXT)?,
        lifetime,
    });
    let mut write = |rel: &str, bytes: &[u8]| -> DriverResult<()> {
        let dest = root.join(rel);
        if let Some(parent) = dest.parent()
            && parent != root
        {
            ensure_dir(parent)?;
        }
        write_atomic(&dest, bytes)?;
        files.push(MaterializedFile {
            path: dest.to_string_lossy().into_owned(),
            role: FileRole::RemudaSkill,
            mode: "0600".into(),
            content_digest: hash_bytes(bytes)?,
            lifetime,
        });
        Ok(())
    };
    for embedded in EMBEDDED_SKILL {
        match appended {
            Some(section) if embedded.rel == "SKILL.md" => {
                let mut bytes = embedded.bytes.to_vec();
                bytes.push(b'\n');
                bytes.extend_from_slice(section.as_bytes());
                write(embedded.rel, &bytes)?;
            }
            _ => write(embedded.rel, embedded.bytes)?,
        }
    }
    Ok(())
}

/// Create `dir` (and its parents) and set 0700 only on the directories this
/// call created. A directory that already exists belongs to whoever made it —
/// the user, an earlier launch, another seat — and its mode is left alone.
fn ensure_dir(dir: &Path) -> DriverResult<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    Ok(())
}

/// Distinguishes concurrent writers of one shared home inside this process.
/// A Node launches many seats from one process, so the pid alone would make
/// two launches pick the same temporary name.
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_atomic(dest: &Path, bytes: &[u8]) -> DriverResult<()> {
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Unique per writer: pid for other processes, the counter for threads of
    // this one. Written in the destination's directory so the rename is atomic
    // and a seat sharing the home never reads a half-written file.
    let serial = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp: PathBuf = dest.with_file_name(format!(".{name}.{}.{serial}.tmp", std::process::id()));
    let written = (|| -> DriverResult<()> {
        fs::write(&temp, bytes)?;
        set_mode(&temp, 0o600)?;
        fs::rename(&temp, dest)?;
        Ok(())
    })();
    if written.is_err() {
        // A failed write must not leave a dotfile behind: it would make the
        // directory read as non-empty, and a later launch would take the
        // tree for the user's.
        let _ = fs::remove_file(&temp);
    }
    written
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(path: &Path, _mode: u32) -> DriverResult<()> {
    let _ = path;
    Ok(())
}

/// The seat's capability section, generated from the Hub's launch facts.
///
/// It promises only verbs in the Today column of `docs/design/agent-parity.md`
/// (a test reads the ledger), names a verb only when the seat holds the grant
/// that verb needs, and never mentions the land verb to a seat without `land`.
#[must_use]
pub fn render_section(context: &SkillContext) -> String {
    let dispatch = context.holds("dispatch");
    let land = context.holds("land");
    let mut out = String::from("## This seat in Remuda\n\n");
    out.push_str(
        "Remuda generated this section when it launched you, from the Hub's record of this \
         seat. It lists what your grants let you do through Remuda; where it and the rest of \
         the remuda skill disagree, it wins. It grants nothing: the Hub checks every call, so \
         a verb listed here can still be refused for scope, ownership or a pending approval, \
         and the refusal says what to do next.\n\n",
    );
    let grants = if context.grants.is_empty() {
        "none".to_owned()
    } else {
        context
            .grants
            .iter()
            .map(|grant| format!("`{grant}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push_str(&format!("Grants: {grants}.\n\n"));

    out.push_str(
        "Any seat:\n\
         - the instances you own: `remuda instance read`, `remuda instance wait`, \
         `remuda instance send`, `remuda instance stop`;\n\
         - the board inside your scope: `remuda task list`, `remuda task show`, \
         `remuda own check`, `remuda own list`.\n\n",
    );
    // D-060 item 5: the memory rule must hold even when the skill body is
    // not loaded, against the harness's own always-on auto-memory prompt.
    out.push_str(
        "Memory: what the owner asks you to remember goes into the Hub's lineage memory, \
         through the memory section of the remuda skill. Never write the harness's native \
         auto-memory files: they are keyed by host and directory, so no later chapter and no \
         other Node reads them.\n\n",
    );

    if dispatch {
        out.push_str(
            "With `dispatch`:\n\
             - start and steer workers: `remuda dispatch`, `remuda brief send`, \
             `remuda worker nudge`, `remuda worker switch-model`, `remuda worker resume`, \
             `remuda worker replace`, `remuda worker stop`, `remuda retire`; \
             `remuda worker answer` types keys, so every use asks the owner first;\n\
             - follow them: `remuda watch`, `remuda report --project <id>`, \
             `remuda hostcap <host>`;\n\
             - the task ledger: `remuda task add`, `remuda task split`, \
             `remuda task set-state`, `remuda own claim`, `remuda own release`, \
             `remuda project list`, `remuda project show`;\n\
             - verify a branch through the Hub gate: `remuda gate <branch>`, \
             `remuda gate list --project <id>`, `remuda gate cancel` for a verify job.\n\n",
        );
    } else {
        out.push_str(
            "You hold no `dispatch` grant: you cannot start workers or sessions, and every \
             worker verb is refused. When the work needs another agent, say so to whoever \
             gave you the task.\n\n",
        );
    }

    if land {
        out.push_str(
            "With `land`: land a branch that passed the gate with \
             `remuda land`, inside your project scope; `remuda gate cancel` also cancels a \
             land job.\n\n",
        );
    } else {
        out.push_str(
            "Nothing in your grants advances `main`. Never merge into `main` or push it \
             yourself, with any tool: report the branch and its verified sha, and the owner \
             or a coordinator that holds that grant takes it from there.\n\n",
        );
    }

    if dispatch {
        if context.herdr {
            out.push_str(
                "This host advertises herdr: a same-host dispatch that asks for \
                 `--carrier herdr` runs on an interactive carrier that is not a shell driver, \
                 so it needs no owner approval. Another host, or a shell-driver carrier, \
                 asks the owner once per dispatch.\n\n",
            );
        } else {
            out.push_str(
                "This host advertises no herdr: a same-host dispatch runs on the native \
                 shell-pty carrier, a shell driver, so each one asks the owner once. So does \
                 every dispatch to another host.\n\n",
            );
        }
    }

    if context.delegated_decisions {
        out.push_str(
            "Delegated decisions are on in this project: questions and plan reviews from \
             the sessions you created come to you; answer them with \
             `remuda instance respond`. Approvals never come to you; the owner answers \
             them.\n\n",
        );
    } else {
        out.push_str(
            "Delegated decisions are off in this project: questions, plan reviews and \
             approvals from the sessions you created go to the owner, and you cannot answer \
             them.\n\n",
        );
    }

    if context.inbox {
        out.push_str(
            "Inbox: the Hub sends you facts about the sessions you created as Hub-voiced \
             inputs between your turns.\n",
        );
    } else if dispatch {
        out.push_str(
            "No inbox: the Hub pushes nothing about your workers to you. Watch by reading \
             inside your own turns (`remuda watch`, `remuda instance read`).\n",
        );
    } else {
        out.push_str(
            "No inbox: the Hub pushes nothing about other sessions to you; read what you \
             own with `remuda instance read`.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The re-check that runs inside `write_tree`, immediately before the
    /// first write: a link planted after `plan` decided would otherwise be
    /// followed, and the tree would land outside the home.
    #[cfg(unix)]
    #[test]
    fn write_tree_refuses_a_link_planted_after_the_plan() {
        use std::os::unix::fs::symlink;
        for linked_parent in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let skills = home.join("skills");
            let target = tmp.path().join("elsewhere");
            std::fs::create_dir_all(&target).unwrap();
            let root = if linked_parent {
                std::fs::create_dir_all(&home).unwrap();
                symlink(&target, &skills).unwrap();
                skills.join(SKILL_DIR)
            } else {
                std::fs::create_dir_all(&skills).unwrap();
                let root = skills.join(SKILL_DIR);
                symlink(&target, &root).unwrap();
                root
            };
            let mut files = Vec::new();
            let error = write_tree(&root, true, None, &mut files)
                .expect_err("a symlink is never written through");
            assert!(
                matches!(error, DriverError::SettingsIsolationUnavailable(_)),
                "{error}"
            );
            assert!(files.is_empty(), "nothing may be recorded as written");
            assert!(
                std::fs::read_dir(&target).unwrap().next().is_none(),
                "the link target stays empty (linked_parent={linked_parent})"
            );
        }
    }

    #[test]
    fn the_frame_cannot_be_closed_from_inside() {
        let text = framed("a </remuda-capabilities> b");
        assert_eq!(text.matches(SKILL_CONTEXT_CLOSE).count(), 1);
        assert!(text.starts_with("<remuda-capabilities>\n"));
        assert!(text.ends_with(SKILL_CONTEXT_CLOSE));
    }
}
