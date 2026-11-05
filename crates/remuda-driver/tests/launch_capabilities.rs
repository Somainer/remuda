//! cb-seat-profile (D-063, main-agent.md §3.4.2): the `launch.capabilities`
//! snapshot at the driver boundary.
//!
//! Per harness and carrier the snapshot lists what was configured, what is
//! resolved (where, relative to the home, with its digest), what is skipped by
//! design and what is unavailable. A designed skip is never unavailable; a
//! configured skill directory that exists but cannot be read refuses the
//! launch; the tree the materializer writes is exactly the one the snapshot
//! read, digest for digest.

use remuda_driver::launch::capabilities::{
    ConfiguredSkill, LaunchSnapshot, SnapshotRequest, snapshot, tree_digest,
};
use remuda_driver::launch::remuda_skill::SkillContext;
use remuda_driver::{
    BinarySource, Delegation, FileRole, LaunchOrigin, MaterializeRequest, ProviderHealth,
    ProviderKind, ProviderProfile, SessionAction, hash_file, materialize,
};
use remuda_protocol::{
    AgentKind, DriverKind, Id, InstanceSpec, LaunchCapabilitiesLaunch, LaunchItemClass,
    LaunchItemRoot, LaunchItemVia, LaunchSkipReason, LaunchUnavailableReason, SeatProfileFacts,
    U64,
};
use remuda_testing::install_executable;
use std::fs;
use std::path::{Path, PathBuf};

fn spec(driver: DriverKind, kind: AgentKind) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = driver;
    spec.kind = kind;
    spec
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["sonnet".into()],
        health: ProviderHealth::Healthy,
    }
}

fn context() -> SkillContext {
    SkillContext {
        grants: vec!["dispatch".to_owned(), "address-owner".to_owned()],
        herdr: true,
        delegated_decisions: true,
        inbox: false,
        profile: Some(SeatProfileFacts {
            revision: U64(3),
            display_name: Some("Main".to_owned()),
            role: None,
            model_candidates: vec!["model-a".to_owned()],
        }),
    }
}

/// An instance dir with `launch/` and a per-instance `native-home`, plus a
/// scratch area for configured skill sources.
struct Root {
    dir: tempfile::TempDir,
    launch: PathBuf,
    home: PathBuf,
}

fn root() -> Root {
    let dir = tempfile::tempdir().unwrap();
    let instance = dir.path().join("instances").join("ins_x");
    let launch = instance.join("launch");
    let home = instance.join("native-home");
    fs::create_dir_all(&launch).unwrap();
    fs::create_dir_all(&home).unwrap();
    Root { dir, launch, home }
}

/// A configured skill source: `SKILL.md` plus one nested reference file.
fn skill_source(at: &Path, name: &str) -> PathBuf {
    let dir = at.join("sources").join(name);
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\n---\nbody\n"),
    )
    .unwrap();
    fs::write(dir.join("references/notes.md"), b"notes\n").unwrap();
    dir
}

struct Ask<'a> {
    driver: DriverKind,
    kind: AgentKind,
    managed: bool,
    skills: Vec<String>,
    args: Vec<String>,
    capabilities: Vec<String>,
    context: Option<&'a SkillContext>,
    launch: LaunchCapabilitiesLaunch,
}

impl<'a> Ask<'a> {
    fn new(driver: DriverKind, kind: AgentKind) -> Self {
        Self {
            driver,
            kind,
            managed: true,
            skills: Vec::new(),
            args: Vec::new(),
            capabilities: Vec::new(),
            context: None,
            launch: LaunchCapabilitiesLaunch::Create,
        }
    }

    fn run(&self, at: &Root) -> LaunchSnapshot {
        let spec = spec(self.driver, self.kind);
        snapshot(&SnapshotRequest {
            launch: self.launch,
            kind: self.kind,
            driver: self.driver,
            origin: LaunchOrigin::Human,
            permission: &spec.permission_mode,
            capabilities: &self.capabilities,
            args: &self.args,
            skills: &self.skills,
            skill_context: self.context,
            cwd: at.dir.path(),
            launch_dir: &at.launch,
            native_home: &at.home,
            native_home_managed: self.managed,
        })
        .expect("an agent kind gets a snapshot")
    }
}

fn names<T>(
    items: &[T],
    pick: impl Fn(&T) -> (LaunchItemClass, String),
) -> Vec<(LaunchItemClass, String)> {
    items.iter().map(pick).collect()
}

#[test]
fn a_claude_seat_lists_configured_resolved_and_the_digest_of_what_lands() {
    let at = root();
    let source = skill_source(at.dir.path(), "review-kit");
    let context = context();
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    ask.skills = vec![source.to_string_lossy().into_owned()];
    ask.context = Some(&context);
    let snap = ask.run(&at);
    let payload = &snap.payload;

    assert_eq!(payload.profile_revision, Some(U64(3)));
    assert_eq!(payload.harness, AgentKind::Claude);
    assert_eq!(payload.driver, DriverKind::ClaudeSdk);
    assert!(payload.refusal.is_none());
    assert!(payload.unavailable.is_empty(), "{:?}", payload.unavailable);
    assert!(payload.skipped.is_empty(), "{:?}", payload.skipped);
    let configured = names(&payload.configured, |item| (item.class, item.name.clone()));
    for expected in [
        (LaunchItemClass::Skill, "remuda"),
        (LaunchItemClass::Grant, "dispatch"),
        (LaunchItemClass::Grant, "address-owner"),
        (LaunchItemClass::Skill, "review-kit"),
    ] {
        assert!(
            configured.contains(&(expected.0, expected.1.to_owned())),
            "{expected:?} in {configured:?}"
        );
    }
    let skill = payload
        .resolved
        .iter()
        .find(|item| item.name == "review-kit")
        .expect("the configured skill resolves");
    assert_eq!(skill.root, Some(LaunchItemRoot::NativeHome));
    assert_eq!(skill.path.as_deref(), Some("skills/review-kit"));
    assert_eq!(skill.via, LaunchItemVia::Tree);
    let remuda = payload
        .resolved
        .iter()
        .find(|item| item.name == "remuda")
        .expect("the remuda tree resolves in a per-instance home");
    assert_eq!(remuda.path.as_deref(), Some("skills/remuda"));
    // The section rides the per-instance tree, so the grants do too.
    for grant in payload
        .resolved
        .iter()
        .filter(|item| item.class == LaunchItemClass::Grant)
    {
        assert_eq!(grant.via, LaunchItemVia::Tree);
        assert_eq!(grant.path, None);
    }

    // The materializer writes exactly the bytes the snapshot read, and the
    // installed tree has the digest the event names.
    let spec = spec(DriverKind::ClaudeSdk, AgentKind::Claude);
    let profile = profile();
    let binary = install_executable(at.dir.path(), "agent", "#!/bin/sh\n");
    let recipe = materialize(&MaterializeRequest {
        creator: remuda_driver::CreatorControl::default(),
        spec: &spec,
        profile: &profile,
        launch_dir: at.launch.clone(),
        native_home: at.home.clone(),
        session: SessionAction::New {
            session_id: "01993ab0-0000-7000-8000-000000000003".to_owned(),
        },
        launch_id: Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(remuda_driver::BinaryPin {
            abs_path: binary.to_string_lossy().into_owned(),
            version: "2.1.291 (Claude Code)".into(),
            sha256: hash_file(&binary).unwrap(),
        }),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: Some(true),
        settings_overlay_path: None,
        secret_policy: None,
        memory_context: None,
        skill_context: Some(&context),
        configured_skills: &snap.skills,
    })
    .unwrap();
    let installed = at.home.join("skills/review-kit");
    assert!(installed.join(".remuda-managed").is_file());
    let mut on_disk = Vec::new();
    for rel in ["SKILL.md", "references/notes.md"] {
        on_disk.push((rel.to_owned(), fs::read(installed.join(rel)).unwrap()));
    }
    assert_eq!(
        skill.digest.as_deref(),
        Some(tree_digest(&on_disk).as_str())
    );
    assert_eq!(
        recipe
            .materialized_files
            .iter()
            .filter(|file| file.role == FileRole::ConfiguredSkill)
            .count(),
        3,
        "two files and the marker"
    );
    // A per-instance home: the launch's cleanup removes the tree's files.
    for file in recipe
        .materialized_files
        .iter()
        .filter(|file| file.role == FileRole::ConfiguredSkill)
    {
        assert_eq!(file.lifetime, remuda_driver::FileLifetime::Launch);
    }
}

#[test]
fn a_designed_skip_is_listed_as_skipped_and_never_unavailable() {
    let at = root();
    let source = skill_source(at.dir.path(), "review-kit");
    let context = context();
    // The inherited operator home: Remuda writes nothing there (D-045).
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    ask.managed = false;
    ask.skills = vec![source.to_string_lossy().into_owned()];
    ask.context = Some(&context);
    let payload = ask.run(&at).payload;
    assert!(payload.unavailable.is_empty(), "{:?}", payload.unavailable);
    assert!(payload.refusal.is_none());
    let skipped = names(&payload.skipped, |item| (item.class, item.name.clone()));
    assert_eq!(
        skipped,
        vec![
            (LaunchItemClass::Skill, "remuda".to_owned()),
            (LaunchItemClass::Skill, "review-kit".to_owned())
        ]
    );
    assert!(
        payload
            .skipped
            .iter()
            .all(|item| item.reason == LaunchSkipReason::OperatorHome)
    );
    // The section rides the materializer's own pair instead.
    assert!(
        payload
            .resolved
            .iter()
            .filter(|item| item.class == LaunchItemClass::Grant)
            .all(|item| item.via == LaunchItemVia::Flag)
    );

    // A user-owned tree at the target name is kept byte for byte: skipped.
    let at = root();
    let source = skill_source(at.dir.path(), "review-kit");
    fs::create_dir_all(at.home.join("skills/review-kit")).unwrap();
    fs::write(at.home.join("skills/review-kit/SKILL.md"), b"mine").unwrap();
    let mut ask = Ask::new(DriverKind::ClaudePty, AgentKind::Claude);
    ask.skills = vec![source.to_string_lossy().into_owned()];
    let snap = ask.run(&at);
    assert!(snap.payload.unavailable.is_empty());
    assert_eq!(snap.payload.skipped[0].reason, LaunchSkipReason::UserOwned);
    assert!(
        snap.skills.is_empty(),
        "nothing to install over the user's tree"
    );
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // A root Node reads every mode; the unreadable fixture would not be.
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("Uid:"))
                .map(|line| line.split_whitespace().nth(1) == Some("0"))
        })
        .unwrap_or(false)
}

#[cfg(unix)]
#[test]
fn an_unreadable_configured_skill_refuses_with_reason_and_next() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        eprintln!("skipped: running as root, every mode is readable");
        return;
    }
    let at = root();
    let readable = skill_source(at.dir.path(), "kit-a");
    let locked = skill_source(at.dir.path(), "kit-b");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    ask.skills = vec![
        readable.to_string_lossy().into_owned(),
        locked.to_string_lossy().into_owned(),
    ];
    let snap = ask.run(&at);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    let payload = &snap.payload;
    assert_eq!(payload.unavailable.len(), 1, "{:?}", payload.unavailable);
    assert_eq!(payload.unavailable[0].name, "kit-b");
    assert_eq!(
        payload.unavailable[0].reason,
        LaunchUnavailableReason::Unreadable
    );
    let refusal = payload.refusal.as_ref().expect("the launch is refused");
    assert_eq!(refusal.reason, "launch-capability-unreadable");
    assert_eq!(refusal.next.action, "ask-owner");
    assert!(refusal.next.hint.contains("kit-b"), "{}", refusal.next.hint);
    assert!(
        snap.refusal_text()
            .unwrap()
            .contains("launch-capability-unreadable"),
        "the create's rejection carries the reason"
    );
    assert!(snap.skills.is_empty(), "a refused launch installs nothing");

    // A file inside an otherwise readable tree counts too.
    let at = root();
    let source = skill_source(at.dir.path(), "kit-c");
    let file = source.join("references/notes.md");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let mut ask = Ask::new(DriverKind::GenericPty, AgentKind::Grok);
    ask.skills = vec![source.to_string_lossy().into_owned()];
    let payload = ask.run(&at).payload;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        payload.unavailable[0].reason,
        LaunchUnavailableReason::Unreadable,
        "unreadable wins over unsupported-by-harness: fail closed"
    );
    assert!(payload.refusal.is_some());

    // And a caller --mcp-config that exists but cannot be read.
    let at = root();
    let config = at.dir.path().join("servers.json");
    fs::write(&config, br#"{"mcpServers":{"docs":{"command":"x"}}}"#).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
    let mut ask = Ask::new(DriverKind::ClaudePrint, AgentKind::Claude);
    ask.args = vec![
        "--mcp-config".to_owned(),
        config.to_string_lossy().into_owned(),
    ];
    let payload = ask.run(&at).payload;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(payload.unavailable[0].class, LaunchItemClass::McpConfig);
    assert_eq!(payload.unavailable[0].name, "servers.json");
    assert!(payload.refusal.is_some());
}

#[test]
fn missing_and_invalid_items_are_unavailable_and_do_not_refuse() {
    let at = root();
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    let missing = at.dir.path().join("sources/gone");
    let file = at.dir.path().join("plain-file");
    fs::write(&file, b"x").unwrap();
    ask.skills = vec![
        missing.to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
        "relative/kit".to_owned(),
        at.dir.path().join("remuda").to_string_lossy().into_owned(),
    ];
    ask.args = vec!["--mcp-config=absent.json".to_owned()];
    ask.capabilities = vec!["teleport".to_owned()];
    let payload = ask.run(&at).payload;
    let reasons: Vec<(String, LaunchUnavailableReason)> = payload
        .unavailable
        .iter()
        .map(|item| (item.name.clone(), item.reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("teleport".to_owned(), LaunchUnavailableReason::InvalidName),
            ("absent.json".to_owned(), LaunchUnavailableReason::Missing),
            ("gone".to_owned(), LaunchUnavailableReason::Missing),
            (
                "plain-file".to_owned(),
                LaunchUnavailableReason::NotADirectory
            ),
            ("kit".to_owned(), LaunchUnavailableReason::InvalidName),
            ("remuda".to_owned(), LaunchUnavailableReason::InvalidName),
        ]
    );
    assert!(payload.refusal.is_none(), "none of these refuses");
}

/// Every agent harness on each carrier the Node registers: the remuda skill
/// and a configured skill land only where a Claude harness reads them.
#[test]
fn each_harness_lists_its_own_delivery() {
    let context = context();
    for (driver, kind, remuda, configured) in [
        (DriverKind::ClaudePrint, AgentKind::Claude, None, None),
        (DriverKind::ClaudeSdk, AgentKind::Claude, None, None),
        (DriverKind::ClaudePty, AgentKind::Claude, None, None),
        (DriverKind::ClaudeBg, AgentKind::Claude, None, None),
        (DriverKind::ShellPty, AgentKind::Claude, None, None),
        // A generic-pty Claude pane with no login material runs on the
        // operator's own home: no reader for anything Remuda writes.
        (
            DriverKind::GenericPty,
            AgentKind::Claude,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Err(LaunchSkipReason::NoSkillReader)),
        ),
        (
            DriverKind::GenericPty,
            AgentKind::Codex,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
        (
            DriverKind::ShellPty,
            AgentKind::Codex,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
        (
            DriverKind::GenericPty,
            AgentKind::Grok,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
        (
            DriverKind::ShellPty,
            AgentKind::Grok,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
        (
            DriverKind::GenericPty,
            AgentKind::Agy,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
        (
            DriverKind::ShellPty,
            AgentKind::Agy,
            Some(LaunchSkipReason::NoSkillReader),
            Some(Ok(LaunchUnavailableReason::UnsupportedByHarness)),
        ),
    ] {
        let at = root();
        let source = skill_source(at.dir.path(), "kit");
        let mut ask = Ask::new(driver, kind);
        ask.context = Some(&context);
        ask.skills = vec![source.to_string_lossy().into_owned()];
        let snap = ask.run(&at);
        let payload = &snap.payload;
        let label = format!("{driver:?}/{kind:?}");
        assert!(payload.refusal.is_none(), "{label}");
        let find_skip = |name: &str| {
            payload
                .skipped
                .iter()
                .find(|item| item.name == name)
                .map(|item| item.reason)
        };
        let find_resolved = |name: &str| payload.resolved.iter().any(|item| item.name == name);
        match remuda {
            None => assert!(find_resolved("remuda"), "{label}: remuda resolved"),
            Some(reason) => assert_eq!(find_skip("remuda"), Some(reason), "{label}"),
        }
        match configured {
            None => {
                assert!(find_resolved("kit"), "{label}: kit resolved");
                assert_eq!(snap.skills.len(), 1, "{label}");
            }
            Some(Err(reason)) => assert_eq!(find_skip("kit"), Some(reason), "{label}"),
            Some(Ok(reason)) => {
                assert_eq!(payload.unavailable[0].name, "kit", "{label}");
                assert_eq!(payload.unavailable[0].reason, reason, "{label}");
                assert!(snap.skills.is_empty(), "{label}");
            }
        }
        // A designed skip never lands in unavailable.
        assert!(
            payload
                .unavailable
                .iter()
                .all(|item| item.reason == LaunchUnavailableReason::UnsupportedByHarness),
            "{label}: {:?}",
            payload.unavailable
        );
        // The grants ride the section: per-instance tree for the Claude argv
        // carriers, the first-input block everywhere else.
        let via = if remuda.is_none() {
            LaunchItemVia::Tree
        } else {
            LaunchItemVia::DataBlock
        };
        assert!(
            payload
                .resolved
                .iter()
                .filter(|item| item.class == LaunchItemClass::Grant)
                .all(|item| item.via == via),
            "{label}"
        );
    }
}

#[test]
fn a_native_resume_on_a_data_block_path_carries_the_earlier_block() {
    let at = root();
    let context = context();
    let mut ask = Ask::new(DriverKind::GenericPty, AgentKind::Codex);
    ask.context = Some(&context);
    ask.launch = LaunchCapabilitiesLaunch::Resume;
    let payload = ask.run(&at).payload;
    assert_eq!(payload.launch, LaunchCapabilitiesLaunch::Resume);
    assert!(
        payload
            .resolved
            .iter()
            .filter(|item| item.class == LaunchItemClass::Grant)
            .all(|item| item.via == LaunchItemVia::Conversation)
    );
}

#[test]
fn terminal_and_generic_kinds_get_no_snapshot() {
    let at = root();
    for kind in [AgentKind::Terminal, AgentKind::Generic] {
        let spec = spec(DriverKind::ShellPty, kind);
        assert!(
            snapshot(&SnapshotRequest {
                launch: LaunchCapabilitiesLaunch::Create,
                kind,
                driver: DriverKind::ShellPty,
                origin: LaunchOrigin::Human,
                permission: &spec.permission_mode,
                capabilities: &[],
                args: &[],
                skills: &[],
                skill_context: None,
                cwd: at.dir.path(),
                launch_dir: &at.launch,
                native_home: &at.home,
                native_home_managed: true,
            })
            .is_none()
        );
    }
}

/// Inputs that carry secrets (an inline MCP config with a token, an MCP
/// config file whose server env holds a key, skill files holding a token)
/// yield an event with names, digests and categories only.
#[test]
fn secret_bearing_inputs_never_reach_the_event() {
    let at = root();
    let token = "sk-ant-api03-SEATPROFxSECRETxVALUEx0123456789abcdefABCDEF0123456789";
    let source = skill_source(at.dir.path(), "kit");
    fs::write(source.join("token.txt"), token).unwrap();
    let config = at.dir.path().join("servers.json");
    fs::write(
        &config,
        format!(
            r#"{{"mcpServers":{{"docs":{{"command":"/opt/x","args":["--key","{token}"],"env":{{"API_KEY":"{token}"}}}}}}}}"#
        ),
    )
    .unwrap();
    let inline = format!(r#"{{"mcpServers":{{"inline-docs":{{"env":{{"TOKEN":"{token}"}}}}}}}}"#);
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    ask.skills = vec![source.to_string_lossy().into_owned()];
    ask.args = vec![
        "--mcp-config".to_owned(),
        config.to_string_lossy().into_owned(),
        format!("--mcp-config={inline}"),
    ];
    let snap = ask.run(&at);
    let text = serde_json::to_string(&snap.payload).unwrap();
    assert!(!text.contains(token), "{text}");
    assert!(!text.contains("/opt/x"), "no MCP command");
    assert!(!text.contains("API_KEY"), "no MCP env names either");
    assert!(
        !text.contains(&at.dir.path().to_string_lossy().into_owned()),
        "no absolute path: {text}"
    );
    for name in ["docs", "inline-docs", "inline", "servers.json", "kit"] {
        assert!(
            text.contains(&format!("\"{name}\"")),
            "{name} listed: {text}"
        );
    }
    // The installed skill still carries its bytes: only the event is bare.
    let kit: &ConfiguredSkill = &snap.skills[0];
    assert!(kit.files.iter().any(|(rel, _)| rel == "token.txt"));
}

/// r2 item 1: the caps are checked before a byte is read. A sparse file
/// claiming a terabyte would exhaust the Node if it were loaded; it is
/// refused `too-large` from its size alone, and so is a tree whose small
/// files add up past the budget.
#[test]
fn an_oversized_file_is_refused_before_it_is_read() {
    use remuda_driver::launch::capabilities::{MAX_SKILL_BYTES, read_skill_tree};
    let at = root();
    let source = skill_source(at.dir.path(), "huge-kit");
    let huge = fs::File::create(source.join("blob.bin")).unwrap();
    huge.set_len(1 << 40).unwrap();
    drop(huge);
    let started = std::time::Instant::now();
    assert_eq!(
        read_skill_tree(&source),
        Err(LaunchUnavailableReason::TooLarge)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "refused from metadata, not by reading a terabyte"
    );
    let mut ask = Ask::new(DriverKind::ClaudeSdk, AgentKind::Claude);
    ask.skills = vec![source.to_string_lossy().into_owned()];
    let snap = ask.run(&at);
    assert_eq!(
        snap.payload.unavailable[0].reason,
        LaunchUnavailableReason::TooLarge
    );
    assert!(snap.payload.refusal.is_none(), "too-large never refuses");

    // Several files, each under the cap, whose sum is over it.
    let at = root();
    let source = skill_source(at.dir.path(), "wide-kit");
    for index in 0..3 {
        let file = fs::File::create(source.join(format!("part-{index}.bin"))).unwrap();
        file.set_len(MAX_SKILL_BYTES / 2).unwrap();
    }
    assert_eq!(
        read_skill_tree(&source),
        Err(LaunchUnavailableReason::TooLarge)
    );
}
