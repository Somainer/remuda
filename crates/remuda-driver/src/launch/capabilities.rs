//! The `launch.capabilities` snapshot (cb-seat-profile, D-063, main-agent.md
//! §3.4.2; protocol.md §5.5).
//!
//! The Node computes it once per agent launch, after it has chosen the native
//! home and before the harness starts, and journals it. It lists what the spec
//! and the Hub asked for (`configured`), what the launch materializes
//! (`resolved`), the designed skips (`skipped`) and what will not reach the
//! harness (`unavailable`). A configured item that exists but cannot be read
//! refuses the launch; every other category is listed and the launch goes on.
//!
//! The configured skill trees are read here, once: the materializer writes
//! exactly the bytes this read ([`ConfiguredSkill`]), so the digest the event
//! names is the digest of what lands in the home.
//!
//! Only names, paths relative to a launch root, digests and categories go
//! into the payload. Never an env value, an MCP server's command, args or env,
//! a configured path, or file contents.

use crate::binary::hash_bytes;
use crate::launch::remuda_skill::{self, SectionDelivery, SkillContext, SkillTree, TreeSkip};
use crate::launch::skills::{self, CAPABILITY_COMPUTER_USE, native_home_is_shared};
use crate::materializer::LaunchOrigin;
use remuda_protocol::{
    AgentKind, ApprovalPolicy, ClaudePermissionMode, DriverKind, LAUNCH_CAPABILITY_UNREADABLE,
    LaunchCapabilitiesLaunch, LaunchCapabilitiesPayload, LaunchConfiguredItem, LaunchItemClass,
    LaunchItemRoot, LaunchItemSource, LaunchItemVia, LaunchRefusal, LaunchRefusalNext,
    LaunchResolvedItem, LaunchSkipReason, LaunchSkippedItem, LaunchUnavailableItem,
    LaunchUnavailableReason, PermissionMode,
};
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// Most files one configured skill tree may hold.
pub const MAX_SKILL_FILES: usize = 256;
/// Most bytes one configured skill tree may hold.
pub const MAX_SKILL_BYTES: u64 = 4 * 1024 * 1024;
/// Deepest directory nesting inside one configured skill tree.
pub const MAX_SKILL_DEPTH: usize = 8;
/// Largest caller `--mcp-config` file the snapshot digests.
pub const MAX_MCP_CONFIG_BYTES: u64 = 1024 * 1024;

/// One configured skill tree, read once, that the materializer installs at
/// `<native_home>/skills/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredSkill {
    /// The directory's base name.
    pub name: String,
    /// `(path relative to the skill root, bytes)`, sorted by path.
    pub files: Vec<(String, Vec<u8>)>,
    /// [`tree_digest`] of [`Self::files`].
    pub digest: String,
}

/// Everything the snapshot needs to know about one launch.
#[derive(Debug, Clone)]
pub struct SnapshotRequest<'a> {
    /// `instance.create` or `instance.resume` (a native resume).
    pub launch: LaunchCapabilitiesLaunch,
    /// The harness kind.
    pub kind: AgentKind,
    /// The carrier.
    pub driver: DriverKind,
    /// Who originated the launch (the D-045 agent gate).
    pub origin: LaunchOrigin,
    /// The launch's resolved permission mode (the D-045 unattended gate).
    pub permission: &'a PermissionMode,
    /// `spec.capabilities`.
    pub capabilities: &'a [String],
    /// `spec.args` (caller `--mcp-config` lives here).
    pub args: &'a [String],
    /// `spec.skills`: host-absolute configured skill directories.
    pub skills: &'a [String],
    /// The Hub's `skillContext`, when stamped.
    pub skill_context: Option<&'a SkillContext>,
    /// The launch cwd; a relative `--mcp-config` path is read against it.
    pub cwd: &'a Path,
    /// `<instance>/launch`.
    pub launch_dir: &'a Path,
    /// The native home the Node chose for this launch.
    pub native_home: &'a Path,
    /// Whether `native_home` is Remuda-managed (false: the inherited
    /// operator home).
    pub native_home_managed: bool,
}

/// The snapshot and the trees to install.
#[derive(Debug, Clone)]
pub struct LaunchSnapshot {
    /// The event payload.
    pub payload: LaunchCapabilitiesPayload,
    /// Configured trees the materializer writes (only those `resolved`).
    pub skills: Vec<ConfiguredSkill>,
}

impl LaunchSnapshot {
    /// The refusal text the create settles with, when the launch is refused.
    #[must_use]
    pub fn refusal_text(&self) -> Option<String> {
        let refusal = self.payload.refusal.as_ref()?;
        Some(format!(
            "launch refused ({}): {}",
            refusal.reason, refusal.next.hint
        ))
    }
}

/// Whether the materializer builds this launch's Claude argv itself (the
/// paths whose capability section rides its own `--append-system-prompt`
/// pair), and whether the harness reads `<native_home>/skills`. The one
/// definition both the snapshot and the materializer read.
#[must_use]
pub fn skill_reader(driver: DriverKind, kind: AgentKind, native_home: &Path) -> (bool, bool) {
    let claude_argv = matches!(
        driver,
        DriverKind::ClaudePrint
            | DriverKind::ClaudeSdk
            | DriverKind::ClaudePty
            | DriverKind::ClaudeBg
    ) || (driver == DriverKind::ShellPty && kind == AgentKind::Claude);
    let reads_skills = claude_argv
        || (driver == DriverKind::GenericPty
            && kind == AgentKind::Claude
            && crate::claude_onboarding::has_login_material(native_home));
    (claude_argv, reads_skills)
}

fn is_agent_kind(kind: AgentKind) -> bool {
    matches!(
        kind,
        AgentKind::Claude | AgentKind::Codex | AgentKind::Grok | AgentKind::Agy
    )
}

struct Lists {
    configured: Vec<LaunchConfiguredItem>,
    resolved: Vec<LaunchResolvedItem>,
    skipped: Vec<LaunchSkippedItem>,
    unavailable: Vec<LaunchUnavailableItem>,
}

impl Lists {
    fn configure(&mut self, class: LaunchItemClass, name: &str, source: LaunchItemSource) {
        self.configured.push(LaunchConfiguredItem {
            class,
            name: name.to_owned(),
            source,
        });
    }

    fn resolve(
        &mut self,
        class: LaunchItemClass,
        name: &str,
        root: Option<LaunchItemRoot>,
        path: Option<String>,
        digest: Option<String>,
        via: LaunchItemVia,
    ) {
        self.resolved.push(LaunchResolvedItem {
            class,
            name: name.to_owned(),
            root,
            path,
            digest,
            via,
        });
    }

    fn skip(&mut self, class: LaunchItemClass, name: &str, reason: LaunchSkipReason) {
        self.skipped.push(LaunchSkippedItem {
            class,
            name: name.to_owned(),
            reason,
        });
    }

    fn unavailable(&mut self, class: LaunchItemClass, name: &str, reason: LaunchUnavailableReason) {
        self.unavailable.push(LaunchUnavailableItem {
            class,
            name: name.to_owned(),
            reason,
        });
    }
}

fn skip_reason(reason: TreeSkip) -> LaunchSkipReason {
    match reason {
        TreeSkip::NoSkillReader => LaunchSkipReason::NoSkillReader,
        TreeSkip::OperatorHome => LaunchSkipReason::OperatorHome,
        TreeSkip::UserOwned => LaunchSkipReason::UserOwned,
    }
}

/// Compute the snapshot for one launch. `None` for kinds `terminal` and
/// `generic`, which get no event.
#[must_use]
pub fn snapshot(request: &SnapshotRequest<'_>) -> Option<LaunchSnapshot> {
    if !is_agent_kind(request.kind) {
        return None;
    }
    let mut lists = Lists {
        configured: Vec::new(),
        resolved: Vec::new(),
        skipped: Vec::new(),
        unavailable: Vec::new(),
    };
    let (claude_argv, reads_skills) =
        skill_reader(request.driver, request.kind, request.native_home);
    remuda_skill_items(request, claude_argv, reads_skills, &mut lists);
    computer_use_items(request, &mut lists);
    mcp_config_items(request, &mut lists);
    let skills = configured_skill_items(request, reads_skills, &mut lists);

    let refusing: Vec<&LaunchUnavailableItem> = lists
        .unavailable
        .iter()
        .filter(|item| item.reason.refuses())
        .collect();
    let refusal = (!refusing.is_empty()).then(|| {
        let names = refusing
            .iter()
            .map(|item| format!("{} {:?}", class_word(item.class), item.name))
            .collect::<Vec<_>>()
            .join(", ");
        LaunchRefusal {
            reason: LAUNCH_CAPABILITY_UNREADABLE.to_owned(),
            next: LaunchRefusalNext {
                action: "ask-owner".to_owned(),
                hint: format!(
                    "the configured {names} exists but the Node's user cannot read it; make it \
                     readable, or remove it from the launch (the lineage profile's skills, or \
                     the create's skills or args), then launch again"
                ),
            },
        }
    });
    let refused = refusal.is_some();
    let profile_revision = request
        .skill_context
        .and_then(|context| context.profile.as_ref())
        .map(|profile| profile.revision);
    Some(LaunchSnapshot {
        payload: LaunchCapabilitiesPayload {
            launch: request.launch,
            harness: request.kind,
            driver: request.driver,
            profile_revision,
            configured: lists.configured,
            resolved: lists.resolved,
            skipped: lists.skipped,
            unavailable: lists.unavailable,
            removed: Vec::new(),
            refusal,
        },
        skills: if refused { Vec::new() } else { skills },
    })
}

fn class_word(class: LaunchItemClass) -> &'static str {
    match class {
        LaunchItemClass::Skill => "skill",
        LaunchItemClass::Capability => "capability",
        LaunchItemClass::McpServer => "MCP server",
        LaunchItemClass::McpConfig => "MCP config",
        LaunchItemClass::Grant => "grant",
    }
}

/// The embedded `remuda` skill and the `skillContext` grants (§3.4.1). The
/// decision is [`remuda_skill::plan`]'s, the same the materializer makes.
fn remuda_skill_items(
    request: &SnapshotRequest<'_>,
    claude_argv: bool,
    reads_skills: bool,
    lists: &mut Lists,
) {
    let Some(context) = request.skill_context else {
        return;
    };
    let Some(plan) = remuda_skill::plan(&remuda_skill::SkillRequest {
        context: Some(context),
        kind: request.kind,
        claude_argv,
        reads_skills,
        resume: request.launch == LaunchCapabilitiesLaunch::Resume,
        launch_dir: request.launch_dir,
        native_home: request.native_home,
        native_home_managed: request.native_home_managed,
    }) else {
        return;
    };
    let name = remuda_skill::SKILL_DIR;
    lists.configure(LaunchItemClass::Skill, name, LaunchItemSource::Hub);
    match &plan.recipe.tree {
        SkillTree::Installed { .. } => lists.resolve(
            LaunchItemClass::Skill,
            name,
            Some(LaunchItemRoot::NativeHome),
            Some(format!("skills/{name}")),
            plan.tree_digest().map(prefixed),
            LaunchItemVia::Tree,
        ),
        SkillTree::Skipped { reason } => {
            lists.skip(LaunchItemClass::Skill, name, skip_reason(*reason));
        }
    }
    let via = match &plan.recipe.section {
        Some(SectionDelivery::Tree) => LaunchItemVia::Tree,
        Some(SectionDelivery::Flag { .. }) => LaunchItemVia::Flag,
        Some(SectionDelivery::DataBlock { .. }) => LaunchItemVia::DataBlock,
        None => LaunchItemVia::Conversation,
    };
    for grant in &context.grants {
        lists.configure(LaunchItemClass::Grant, grant, LaunchItemSource::Hub);
        lists.resolve(LaunchItemClass::Grant, grant, None, None, None, via);
    }
}

/// `computer-use` (D-045): the capability, its embedded skill tree and its MCP
/// server. The gates are the materializer's; a launch they refuse is refused
/// there exactly as before, and the event names the category.
fn computer_use_items(request: &SnapshotRequest<'_>, lists: &mut Lists) {
    for value in request.capabilities {
        lists.configure(LaunchItemClass::Capability, value, LaunchItemSource::Spec);
        if value != CAPABILITY_COMPUTER_USE {
            lists.unavailable(
                LaunchItemClass::Capability,
                value,
                LaunchUnavailableReason::InvalidName,
            );
        }
    }
    if !request
        .capabilities
        .iter()
        .any(|value| value == CAPABILITY_COMPUTER_USE)
    {
        return;
    }
    let skill = skills::SKILL_DIR;
    let server = skills::MCP_SERVER_NAME;
    lists.configure(LaunchItemClass::Skill, skill, LaunchItemSource::Embedded);
    lists.configure(
        LaunchItemClass::McpServer,
        server,
        LaunchItemSource::Embedded,
    );
    if let Some(reason) = computer_use_gate(request) {
        for (class, name) in [
            (LaunchItemClass::Capability, CAPABILITY_COMPUTER_USE),
            (LaunchItemClass::Skill, skill),
            (LaunchItemClass::McpServer, server),
        ] {
            lists.unavailable(class, name, reason);
        }
        return;
    }
    let via = if request.kind == AgentKind::Claude {
        LaunchItemVia::Argv
    } else {
        LaunchItemVia::CodexConfig
    };
    lists.resolve(
        LaunchItemClass::Capability,
        CAPABILITY_COMPUTER_USE,
        None,
        None,
        None,
        via,
    );
    if request.kind != AgentKind::Claude {
        lists.skip(
            LaunchItemClass::Skill,
            skill,
            LaunchSkipReason::NoSkillReader,
        );
    } else if !request.native_home_managed {
        lists.skip(
            LaunchItemClass::Skill,
            skill,
            LaunchSkipReason::OperatorHome,
        );
    } else {
        lists.resolve(
            LaunchItemClass::Skill,
            skill,
            Some(LaunchItemRoot::NativeHome),
            Some(format!("skills/{skill}")),
            Some(prefixed(skills::skill_tree_digest(
                skills::embedded_skill_files(),
            ))),
            LaunchItemVia::Tree,
        );
    }
    lists.resolve(
        LaunchItemClass::McpServer,
        server,
        Some(LaunchItemRoot::LaunchDir),
        Some("mcp-cua.json".to_owned()),
        None,
        via,
    );
}

/// The category the D-045 gates give a requested `computer-use`, if any.
fn computer_use_gate(request: &SnapshotRequest<'_>) -> Option<LaunchUnavailableReason> {
    if skills::computer_use_requested(
        &[CAPABILITY_COMPUTER_USE.to_owned()],
        request.origin,
        request.permission,
        request.kind,
        request.driver,
    )
    .is_ok()
    {
        return None;
    }
    let unattended = match request.permission {
        PermissionMode::Claude(claude) => claude.mode == ClaudePermissionMode::BypassPermissions,
        PermissionMode::Codex(codex) => codex.approval_policy == ApprovalPolicy::Never,
        _ => false,
    };
    if matches!(request.origin, LaunchOrigin::Agent) || unattended {
        Some(LaunchUnavailableReason::NotPermitted)
    } else {
        Some(LaunchUnavailableReason::UnsupportedByHarness)
    }
}

/// The values of every `--mcp-config` in `args` (both spellings).
fn mcp_config_values(args: &[String]) -> Vec<&str> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--mcp-config" {
            if let Some(value) = iter.next() {
                values.push(value.as_str());
            }
        } else if let Some(value) = arg.strip_prefix("--mcp-config=") {
            values.push(value);
        }
    }
    values
}

/// Caller `--mcp-config` values: a file the harness reads at startup, or an
/// inline JSON string. A file that exists but cannot be read refuses.
fn mcp_config_items(request: &SnapshotRequest<'_>, lists: &mut Lists) {
    for value in mcp_config_values(request.args) {
        let trimmed = value.trim_start();
        if trimmed.starts_with('{') {
            // Inline JSON may carry tokens: only its digest and server names.
            let name = "inline";
            lists.configure(LaunchItemClass::McpConfig, name, LaunchItemSource::Args);
            lists.resolve(
                LaunchItemClass::McpConfig,
                name,
                None,
                None,
                hash_bytes(value.as_bytes()).ok().map(String::from),
                LaunchItemVia::Argv,
            );
            mcp_server_items(value.as_bytes(), lists);
            continue;
        }
        let path = Path::new(value);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mcp-config".to_owned());
        lists.configure(LaunchItemClass::McpConfig, &name, LaunchItemSource::Args);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            request.cwd.join(path)
        };
        match read_bounded_file(&full, MAX_MCP_CONFIG_BYTES) {
            Ok(bytes) => {
                lists.resolve(
                    LaunchItemClass::McpConfig,
                    &name,
                    None,
                    None,
                    hash_bytes(&bytes).ok().map(String::from),
                    LaunchItemVia::Argv,
                );
                mcp_server_items(&bytes, lists);
            }
            Err(reason) => lists.unavailable(LaunchItemClass::McpConfig, &name, reason),
        }
    }
}

/// The server names an MCP config declares (`mcpServers` keys), never their
/// command, args or env.
fn mcp_server_items(bytes: &[u8], lists: &mut Lists) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    let Some(servers) = value.get("mcpServers").and_then(|value| value.as_object()) else {
        return;
    };
    for name in servers.keys() {
        lists.configure(LaunchItemClass::McpServer, name, LaunchItemSource::Args);
        lists.resolve(
            LaunchItemClass::McpServer,
            name,
            None,
            None,
            None,
            LaunchItemVia::Argv,
        );
    }
}

fn io_reason(error: &std::io::Error) -> LaunchUnavailableReason {
    match error.kind() {
        ErrorKind::NotFound => LaunchUnavailableReason::Missing,
        _ => LaunchUnavailableReason::Unreadable,
    }
}

fn read_bounded_file(path: &Path, max: u64) -> Result<Vec<u8>, LaunchUnavailableReason> {
    let meta = fs::metadata(path).map_err(|error| io_reason(&error))?;
    if !meta.is_file() {
        return Err(LaunchUnavailableReason::NotAFile);
    }
    if meta.len() > max {
        return Err(LaunchUnavailableReason::TooLarge);
    }
    fs::read(path).map_err(|error| io_reason(&error))
}

/// `spec.skills`: read each tree, and for kind `claude` decide where it goes
/// under the §3.4.1 rules for the `remuda` tree.
fn configured_skill_items(
    request: &SnapshotRequest<'_>,
    reads_skills: bool,
    lists: &mut Lists,
) -> Vec<ConfiguredSkill> {
    let mut install = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in request.skills {
        let path = Path::new(raw);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let shown = if name.is_empty() { "skill" } else { &name };
        lists.configure(LaunchItemClass::Skill, shown, LaunchItemSource::Spec);
        if !path.is_absolute()
            || !remuda_protocol::valid_skill_name(&name)
            || !seen.insert(name.clone())
        {
            lists.unavailable(
                LaunchItemClass::Skill,
                shown,
                LaunchUnavailableReason::InvalidName,
            );
            continue;
        }
        let tree = match read_skill_tree(path) {
            Ok(tree) => tree,
            Err(reason) => {
                lists.unavailable(LaunchItemClass::Skill, &name, reason);
                continue;
            }
        };
        if request.kind != AgentKind::Claude {
            lists.unavailable(
                LaunchItemClass::Skill,
                &name,
                LaunchUnavailableReason::UnsupportedByHarness,
            );
            continue;
        }
        let root = request.native_home.join("skills").join(&name);
        let skip = if !reads_skills {
            Some(LaunchSkipReason::NoSkillReader)
        } else if !request.native_home_managed {
            Some(LaunchSkipReason::OperatorHome)
        } else if remuda_skill::tree_blocked(&root) {
            Some(LaunchSkipReason::UserOwned)
        } else {
            None
        };
        if let Some(reason) = skip {
            lists.skip(LaunchItemClass::Skill, &name, reason);
            continue;
        }
        let digest = tree_digest(&tree);
        lists.resolve(
            LaunchItemClass::Skill,
            &name,
            Some(LaunchItemRoot::NativeHome),
            Some(format!("skills/{name}")),
            Some(digest.clone()),
            LaunchItemVia::Tree,
        );
        install.push(ConfiguredSkill {
            name,
            files: tree,
            digest,
        });
    }
    install
}

/// Read a configured skill tree: regular files only, symlinks inside never
/// followed (and not copied), bounded in files, bytes and depth. Any
/// directory or file that cannot be read is `unreadable`.
pub fn read_skill_tree(root: &Path) -> Result<Vec<(String, Vec<u8>)>, LaunchUnavailableReason> {
    let meta = fs::metadata(root).map_err(|error| io_reason(&error))?;
    if !meta.is_dir() {
        return Err(LaunchUnavailableReason::NotADirectory);
    }
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| io_reason_inside(&error))?;
        for entry in entries {
            let entry = entry.map_err(|error| io_reason_inside(&error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| io_reason_inside(&error))?;
            let path = entry.path();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth + 1 > MAX_SKILL_DEPTH {
                    return Err(LaunchUnavailableReason::TooLarge);
                }
                stack.push((path, depth + 1));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            // The caps are checked BEFORE a byte is read: a configured tree
            // holding one huge (or sparse) file must not be pulled into the
            // Node's memory. `DirEntry::metadata` is an lstat, never followed.
            let len = entry
                .metadata()
                .map_err(|error| io_reason_inside(&error))?
                .len();
            if files.len() >= MAX_SKILL_FILES || total.saturating_add(len) > MAX_SKILL_BYTES {
                return Err(LaunchUnavailableReason::TooLarge);
            }
            // Bounded by what is left of the budget, so a file that grows
            // between the stat and the read is still refused, never loaded.
            let bytes = read_at_most(&path, MAX_SKILL_BYTES - total)?;
            total += bytes.len() as u64;
            let rel = relative(root, &path).ok_or(LaunchUnavailableReason::InvalidName)?;
            files.push((rel, bytes));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Read `path`, refusing it (`too-large`) the moment it holds more than
/// `limit` bytes; at most `limit + 1` bytes are ever read.
fn read_at_most(path: &Path, limit: u64) -> Result<Vec<u8>, LaunchUnavailableReason> {
    use std::io::Read as _;
    let file = fs::File::open(path).map_err(|error| io_reason_inside(&error))?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_reason_inside(&error))?;
    if bytes.len() as u64 > limit {
        return Err(LaunchUnavailableReason::TooLarge);
    }
    Ok(bytes)
}

/// Inside a tree that exists, a vanished entry is a read failure too.
fn io_reason_inside(_error: &std::io::Error) -> LaunchUnavailableReason {
    LaunchUnavailableReason::Unreadable
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// `sha256:<hex>` of a tree, the event's digest spelling.
fn prefixed(hex: String) -> String {
    format!("sha256:{hex}")
}

/// The event digest of a configured tree: `skill_tree_digest` over its
/// `(path, bytes)` list, prefixed.
#[must_use]
pub fn tree_digest(files: &[(String, Vec<u8>)]) -> String {
    prefixed(skills::skill_tree_digest(
        files
            .iter()
            .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice())),
    ))
}

/// Remove the configured skill trees an earlier launch installed in
/// `native_home` that `keep` (this launch's resolved names) no longer names
/// (cb-seat-profile r2): a skill dropped from the profile or the spec must
/// not keep loading from a shared home, whose files outlive their launch.
///
/// Only `<native_home>/skills/<name>` directories that are real directories
/// (never a link, never followed) holding Remuda's `.remuda-managed` marker
/// as a regular file are removed; a tree without the marker is the user's
/// and is never touched, and neither are the `remuda` and
/// `codex-computer-use` trees, which other launch paths own. Returns the
/// names removed, sorted. A tree that cannot be removed is logged and left.
#[must_use]
pub fn remove_stale_skills(native_home: &Path, keep: &[String]) -> Vec<String> {
    let skills = native_home.join("skills");
    let Ok(meta) = fs::symlink_metadata(&skills) else {
        return Vec::new();
    };
    if !meta.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&skills) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if keep.contains(&name) || remuda_protocol::RESERVED_SKILL_NAMES.contains(&name.as_str()) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let tree = entry.path();
        let marked = fs::symlink_metadata(tree.join(remuda_skill::MANAGED_MARKER))
            .is_ok_and(|marker| marker.file_type().is_file());
        if !marked {
            continue;
        }
        match fs::remove_dir_all(&tree) {
            Ok(()) => removed.push(name),
            Err(error) => {
                tracing::warn!(skill = %name, %error, "could not remove a revoked skill tree");
            }
        }
    }
    removed.sort();
    removed
}

/// Whether the home a configured tree lands in is shared (its files outlive
/// the launch).
pub(crate) fn configured_tree_shared(launch_dir: &Path, native_home: &Path) -> bool {
    native_home_is_shared(launch_dir, native_home)
}
