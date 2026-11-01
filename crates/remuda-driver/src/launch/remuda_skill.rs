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
//!   `CLAUDE_CONFIG_DIR` to the native home. It follows the computer-use
//!   rules: never the inherited operator home (D-045), and file lifetime by
//!   [`native_home_is_shared`]. It also never overwrites a tree it did not
//!   write: a `skills/remuda` without Remuda's marker is user-owned and left
//!   alone.
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
    /// non-Claude harness, and kind `claude` on `generic-pty` (the pane runs
    /// with the operator's own home).
    NoSkillReader,
    /// The native home is the inherited operator home, which Remuda never
    /// writes (D-045).
    OperatorHome,
    /// `skills/remuda` already exists without Remuda's marker.
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
    /// The materializer builds this launch's Claude argv itself (and pins
    /// `CLAUDE_CONFIG_DIR` to `native_home`).
    pub claude_argv: bool,
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
    let tree = if !(request.claude_argv && request.kind == remuda_protocol::AgentKind::Claude) {
        SkillTree::Skipped {
            reason: TreeSkip::NoSkillReader,
        }
    } else if !request.native_home_managed {
        SkillTree::Skipped {
            reason: TreeSkip::OperatorHome,
        }
    } else {
        let root = request.native_home.join("skills").join(SKILL_DIR);
        if user_owned(&root) {
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

/// A `skills/remuda` that holds something without Remuda's marker. An empty
/// directory is not the user's: it is what a launch's own cleanup leaves when
/// it shreds a per-instance tree, and the next launch in that home rewrites it.
fn user_owned(root: &Path) -> bool {
    if fs::symlink_metadata(root).is_err() || root.join(MANAGED_MARKER).is_file() {
        return false;
    }
    fs::read_dir(root).map_or(true, |mut entries| entries.next().is_some())
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
    fs::create_dir_all(root)?;
    set_mode(skills_root, 0o700)?;
    set_mode(root, 0o700)?;
    let lifetime = if shared {
        FileLifetime::NativeStore
    } else {
        FileLifetime::Launch
    };
    let mut write = |rel: &str, bytes: &[u8]| -> DriverResult<()> {
        let dest = root.join(rel);
        if let Some(parent) = dest.parent()
            && parent != root
        {
            fs::create_dir_all(parent)?;
            set_mode(parent, 0o700)?;
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
    write(MANAGED_MARKER, MARKER_TEXT)?;
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

fn write_atomic(dest: &Path, bytes: &[u8]) -> DriverResult<()> {
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp: PathBuf = dest.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&temp, bytes)?;
    set_mode(&temp, 0o600)?;
    fs::rename(&temp, dest)?;
    Ok(())
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

    #[test]
    fn the_frame_cannot_be_closed_from_inside() {
        let text = framed("a </remuda-capabilities> b");
        assert_eq!(text.matches(SKILL_CONTEXT_CLOSE).count(), 1);
        assert!(text.starts_with("<remuda-capabilities>\n"));
        assert!(text.ends_with(SKILL_CONTEXT_CLOSE));
    }
}
