//! ma-skill (D-060 item 5): the embedded `remuda` skill and the seat's
//! capability section, at the materializer boundary.
//!
//! The embedded tree matches the shipped directory byte for byte. Kind
//! `claude` on the Claude argv carriers gets the tree in a Remuda-managed
//! home (with the section appended when the home is the seat's alone, the
//! section on its own system-prompt pair when the home is shared), never in
//! the inherited operator home and never over a user-owned tree; every other
//! harness gets the section as a first-input data block. The section itself
//! follows the launch facts and names only verbs the parity ledger's Today
//! column lists.

use remuda_driver::launch::remuda_skill::{
    MANAGED_MARKER, SOURCE_ONLY, SectionDelivery, SkillContext, SkillTree, TreeSkip,
    embedded_skill_files, render_section,
};
use remuda_driver::{
    APPEND_SYSTEM_PROMPT, BinarySource, Delegation, FileLifetime, FileRole, LaunchOrigin,
    LaunchRecipe, MaterializeRequest, MemoryContext, ProviderHealth, ProviderKind, ProviderProfile,
    SessionAction, hash_file, materialize,
};
use remuda_protocol::{AgentKind, DriverKind, Id, InstanceSpec, U64};
use remuda_testing::install_executable;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SKILL_ROOT: &str = "skills/remuda";
const OPEN: &str = "<remuda-capabilities>";

// ── embedded bytes vs the shipped skill directory ─────────────────────────

/// Walk a directory into sorted `(relative path, bytes)` pairs.
fn walk_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, fs::read(&path).unwrap()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn source_only(rel: &str) -> bool {
    SOURCE_ONLY
        .iter()
        .any(|entry| match entry.strip_suffix('/') {
            Some(dir) => rel.starts_with(&format!("{dir}/")),
            None => rel == *entry,
        })
}

/// Every shipped file under `skills/remuda/` (everything but the section
/// sources and the maintainers' README) is embedded, and every embedded file
/// is shipped, with the same bytes: a new reference file, a stale embed or a
/// regenerated `SKILL.md` the binary did not pick up fails here.
#[test]
fn embedded_skill_matches_the_shipped_directory_byte_for_byte() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(SKILL_ROOT);
    let shipped: Vec<(String, Vec<u8>)> = walk_files(&source)
        .into_iter()
        .filter(|(rel, _)| !source_only(rel))
        .collect();
    let mut embedded: Vec<(String, Vec<u8>)> = embedded_skill_files()
        .map(|(rel, bytes)| (rel.to_owned(), bytes.to_vec()))
        .collect();
    embedded.sort_by(|a, b| a.0.cmp(&b.0));
    let names =
        |set: &[(String, Vec<u8>)]| set.iter().map(|(rel, _)| rel.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(&shipped),
        names(&embedded),
        "the embedded set must list every shipped file"
    );
    let digest = |set: &[(String, Vec<u8>)]| {
        remuda_driver::launch::skills::skill_tree_digest(
            set.iter()
                .map(|(rel, bytes)| (rel.as_str(), bytes.as_slice())),
        )
    };
    assert_eq!(digest(&shipped), digest(&embedded));
    // The sources the README describes are present, so the filter above is
    // not hiding a renamed shipped file.
    assert!(source.join("sections/00-intro.md").is_file());
    assert!(source.join("README.md").is_file());
}

// ── materializer fixtures ──────────────────────────────────────────────────

fn profile(driver: DriverKind) -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: match driver {
            DriverKind::CodexAppserver => ProviderKind::OpenaiResponses,
            DriverKind::GrokAcp => ProviderKind::Xai,
            DriverKind::AgyPrint => ProviderKind::Google,
            _ => ProviderKind::Anthropic,
        },
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["sonnet".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec(driver: DriverKind, kind: AgentKind) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.driver = driver;
    spec.kind = kind;
    spec
}

/// One launch: an instance dir holding `launch/`, and a native home that is
/// either inside it (per-instance) or beside it (shared).
struct Launch {
    _root: tempfile::TempDir,
    launch: PathBuf,
    home: PathBuf,
    binary: PathBuf,
}

fn launch(shared: bool) -> Launch {
    let root = tempfile::tempdir().unwrap();
    let instance = root.path().join("instances").join("ins_x");
    let launch = instance.join("launch");
    let home = if shared {
        root.path().join("shared-home")
    } else {
        instance.join("native-home")
    };
    fs::create_dir_all(&launch).unwrap();
    fs::create_dir_all(&home).unwrap();
    let binary = install_executable(root.path(), "agent", "#!/bin/sh\n");
    Launch {
        _root: root,
        launch,
        home,
        binary,
    }
}

struct Run<'a> {
    driver: DriverKind,
    kind: AgentKind,
    managed: bool,
    resume: bool,
    skill: Option<&'a SkillContext>,
    memory: Option<&'a MemoryContext>,
}

impl<'a> Run<'a> {
    fn new(driver: DriverKind, kind: AgentKind, skill: Option<&'a SkillContext>) -> Self {
        Self {
            driver,
            kind,
            managed: true,
            resume: false,
            skill,
            memory: None,
        }
    }

    fn materialize(&self, at: &Launch) -> LaunchRecipe {
        let spec = spec(self.driver, self.kind);
        let profile = profile(self.driver);
        let session_id = "01993ab0-0000-7000-8000-000000000003".to_owned();
        materialize(&MaterializeRequest {
            creator: remuda_driver::CreatorControl::default(),
            spec: &spec,
            profile: &profile,
            launch_dir: at.launch.clone(),
            native_home: at.home.clone(),
            session: if self.resume {
                SessionAction::Resume { session_id }
            } else {
                SessionAction::New { session_id }
            },
            launch_id: Id::new("launch").unwrap(),
            binary: BinarySource::Pinned(remuda_driver::BinaryPin {
                abs_path: at.binary.to_string_lossy().into_owned(),
                version: "2.1.291 (Claude Code)".into(),
                sha256: hash_file(&at.binary).unwrap(),
            }),
            setting_sources: None,
            origin: LaunchOrigin::Human,
            native_home_managed: Some(self.managed),
            settings_overlay_path: None,
            secret_policy: None,
            memory_context: self.memory,
            skill_context: self.skill,
        })
        .unwrap_or_else(|error| panic!("{:?}/{:?}: {error}", self.driver, self.kind))
    }
}

fn facts(grants: &[&str]) -> SkillContext {
    SkillContext {
        grants: grants.iter().map(|grant| (*grant).to_owned()).collect(),
        herdr: true,
        delegated_decisions: true,
        inbox: false,
    }
}

fn embedded_skill_md() -> &'static [u8] {
    embedded_skill_files()
        .find(|(rel, _)| *rel == "SKILL.md")
        .unwrap()
        .1
}

fn pairs(argv: &[String]) -> Vec<usize> {
    argv.iter()
        .enumerate()
        .filter(|(_, token)| token.starts_with(APPEND_SYSTEM_PROMPT))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ── install behaviour per harness ─────────────────────────────────────────

/// A per-instance home is the seat's alone: the tree goes in, owner-only, with
/// the section appended to `SKILL.md`, and dies with the launch. Nothing rides
/// argv for it.
#[test]
fn claude_on_a_per_instance_home_gets_the_tree_with_the_section_appended() {
    let context = facts(&["dispatch", "land"]);
    for driver in [
        DriverKind::ClaudePrint,
        DriverKind::ClaudeSdk,
        DriverKind::ClaudePty,
        DriverKind::ClaudeBg,
        DriverKind::ShellPty,
    ] {
        let at = launch(false);
        let recipe = Run::new(driver, AgentKind::Claude, Some(&context)).materialize(&at);
        let root = at.home.join(SKILL_ROOT);
        let skill = recipe.skill.as_ref().expect("skill recorded");
        assert_eq!(
            skill.tree,
            SkillTree::Installed {
                root: root.to_string_lossy().into_owned(),
                shared: false
            },
            "{driver:?}"
        );
        assert_eq!(skill.section, Some(SectionDelivery::Tree), "{driver:?}");
        assert_eq!(skill.context, context);
        let installed = fs::read(root.join("SKILL.md")).unwrap();
        let mut expected = embedded_skill_md().to_vec();
        expected.push(b'\n');
        expected.extend_from_slice(render_section(&context).as_bytes());
        assert_eq!(installed, expected, "{driver:?}");
        assert!(root.join(MANAGED_MARKER).is_file());
        assert!(
            pairs(&recipe.argv).is_empty(),
            "{driver:?}: {:?}",
            recipe.argv
        );
        assert_eq!(recipe.own_system_prompt(), None);
        assert_eq!(recipe.first_input_block(), None);
        #[cfg(unix)]
        {
            assert_eq!(mode(&root), 0o700);
            assert_eq!(mode(&at.home.join("skills")), 0o700);
            assert_eq!(mode(&root.join("SKILL.md")), 0o600);
        }
        let written: Vec<_> = recipe
            .materialized_files
            .iter()
            .filter(|file| file.role == FileRole::RemudaSkill)
            .collect();
        assert_eq!(written.len(), 2, "{driver:?}: SKILL.md and the marker");
        assert!(
            written
                .iter()
                .all(|file| file.lifetime == FileLifetime::Launch)
        );
        // The launch's cleanup removes what it wrote, and the next launch in
        // the same home (each claude-bg process, a respawn) writes it again:
        // the empty directory cleanup leaves is not a user-owned tree.
        recipe.cleanup_launch_files();
        assert!(!root.join("SKILL.md").exists(), "{driver:?}");
        assert!(root.is_dir());
        let again = Run::new(driver, AgentKind::Claude, Some(&context)).materialize(&at);
        assert!(
            matches!(
                again.skill.as_ref().unwrap().tree,
                SkillTree::Installed { shared: false, .. }
            ),
            "{driver:?}: {:?}",
            again.skill
        );
        assert_eq!(fs::read(root.join("SKILL.md")).unwrap(), expected);
    }
}

/// A shared home (a Node-wide or an explicit config dir) outlives the launch
/// and serves other seats: the tree is the embedded bytes, kept with the
/// home, and the seat's section is skipped from it; it rides the launch's own
/// system-prompt pair instead, after the memory index.
#[test]
fn claude_on_a_shared_home_keeps_its_section_out_of_the_tree() {
    let context = facts(&["dispatch"]);
    let memory = MemoryContext {
        rev: U64(4),
        text: "<remuda-memory rev=\"4\">\n## Index\n</remuda-memory>".into(),
    };
    for driver in [DriverKind::ClaudeSdk, DriverKind::ShellPty] {
        let at = launch(true);
        let mut run = Run::new(driver, AgentKind::Claude, Some(&context));
        run.memory = Some(&memory);
        let recipe = run.materialize(&at);
        let root = at.home.join(SKILL_ROOT);
        let skill = recipe.skill.as_ref().unwrap();
        assert_eq!(
            skill.tree,
            SkillTree::Installed {
                root: root.to_string_lossy().into_owned(),
                shared: true
            }
        );
        assert_eq!(
            fs::read(root.join("SKILL.md")).unwrap(),
            embedded_skill_md()
        );
        let section = skill.flag_text().expect("the section rides the pair");
        assert!(section.starts_with(OPEN) && section.contains("## This seat in Remuda"));
        let at_pair = pairs(&recipe.argv);
        assert_eq!(at_pair.len(), 1, "{driver:?}: {:?}", recipe.argv);
        let value = &recipe.argv[at_pair[0] + 1];
        assert_eq!(value, &format!("{}\n\n{section}", memory.text));
        assert_eq!(recipe.own_system_prompt(), Some(value.as_str()));
        assert!(
            recipe
                .materialized_files
                .iter()
                .filter(|file| file.role == FileRole::RemudaSkill)
                .all(|file| file.lifetime == FileLifetime::NativeStore)
        );
        recipe.cleanup_launch_files();
        assert!(root.join("SKILL.md").is_file(), "a shared tree survives");
    }
}

/// The inherited operator home is never written (D-045): no tree, and the
/// section rides the pair, alone when there is no memory. A native resume
/// renders it fresh, like the memory pair.
#[test]
fn claude_on_the_inherited_operator_home_writes_nothing_into_it() {
    let context = facts(&[]);
    let at = launch(true);
    let mut run = Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context));
    run.managed = false;
    let recipe = run.materialize(&at);
    assert!(!at.home.join("skills").exists());
    let skill = recipe.skill.as_ref().unwrap();
    assert_eq!(
        skill.tree,
        SkillTree::Skipped {
            reason: TreeSkip::OperatorHome
        }
    );
    let section = skill.flag_text().unwrap().to_owned();
    assert_eq!(recipe.own_system_prompt(), Some(section.as_str()));
    assert!(
        !recipe
            .argv
            .iter()
            .any(|token| token == "--system-prompt-snapshot")
    );

    run.resume = true;
    let resumed = run.materialize(&at);
    let at_pair = pairs(&resumed.argv);
    assert_eq!(at_pair.len(), 1);
    assert_eq!(resumed.argv[at_pair[0] + 1], section);
    assert_eq!(
        &resumed.argv[at_pair[0] - 2..at_pair[0]],
        ["--system-prompt-snapshot", "off"]
    );
}

/// A `skills/remuda` Remuda did not write is the user's: left byte for byte,
/// and the section rides the pair. One with Remuda's marker is refreshed.
#[test]
fn a_user_owned_tree_is_never_overwritten() {
    let context = facts(&["land"]);
    let at = launch(false);
    let root = at.home.join(SKILL_ROOT);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("SKILL.md"), b"the user's own remuda skill\n").unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(
        fs::read(root.join("SKILL.md")).unwrap(),
        b"the user's own remuda skill\n"
    );
    assert!(!root.join(MANAGED_MARKER).exists());
    let skill = recipe.skill.as_ref().unwrap();
    assert_eq!(
        skill.tree,
        SkillTree::Skipped {
            reason: TreeSkip::UserOwned
        }
    );
    assert!(skill.flag_text().is_some());
    assert!(
        recipe
            .materialized_files
            .iter()
            .all(|file| file.role != FileRole::RemudaSkill)
    );

    // Remuda's own earlier tree is rewritten.
    fs::write(root.join(MANAGED_MARKER), b"remuda").unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert!(matches!(
        recipe.skill.unwrap().tree,
        SkillTree::Installed { .. }
    ));
    assert!(
        fs::read(root.join("SKILL.md"))
            .unwrap()
            .starts_with(embedded_skill_md())
    );
}

/// Every harness that reads no skills directory Remuda writes — codex, grok
/// and agy on every carrier, and claude in a raw `generic-pty` pane — gets the
/// section as framed data ahead of its first input, after the memory block,
/// and no tree; a native resume does not send it twice.
#[test]
fn other_harnesses_get_the_section_as_a_first_input_data_block() {
    let context = facts(&["dispatch"]);
    let memory = MemoryContext {
        rev: U64(2),
        text: "<remuda-memory rev=\"2\">\n## Index\n</remuda-memory>".into(),
    };
    for (driver, kind) in [
        (DriverKind::CodexAppserver, AgentKind::Codex),
        (DriverKind::GrokAcp, AgentKind::Grok),
        (DriverKind::AgyPrint, AgentKind::Agy),
        // A generic-pty Claude pane is pinned to the home only when the home
        // holds a login; this one does not, so there is no skills reader.
        (DriverKind::GenericPty, AgentKind::Claude),
        (DriverKind::GenericPty, AgentKind::Codex),
        (DriverKind::ShellPty, AgentKind::Codex),
        (DriverKind::ShellPty, AgentKind::Grok),
        (DriverKind::ShellPty, AgentKind::Agy),
    ] {
        let at = launch(false);
        let mut run = Run::new(driver, kind, Some(&context));
        let recipe = run.materialize(&at);
        assert!(!at.home.join("skills").exists(), "{driver:?}/{kind:?}");
        let skill = recipe.skill.as_ref().unwrap();
        assert_eq!(
            skill.tree,
            SkillTree::Skipped {
                reason: TreeSkip::NoSkillReader
            },
            "{driver:?}/{kind:?}"
        );
        let block = skill.data_block().expect("data block");
        assert!(block.starts_with(OPEN), "{block}");
        assert_eq!(recipe.first_input_block().as_deref(), Some(block));
        assert!(pairs(&recipe.argv).is_empty(), "{driver:?}/{kind:?}");

        run.memory = Some(&memory);
        let both = run.materialize(&at);
        assert_eq!(
            both.first_input_block(),
            Some(format!("{}\n\n{block}", memory.text)),
            "{driver:?}/{kind:?}"
        );
        // What the sender typed is recovered from the composed first input.
        let delivered = remuda_driver::memory_context::with_data_block(
            &both.first_input_block().unwrap(),
            "hi",
        );
        assert_eq!(
            remuda_protocol::hubnode::strip_memory_context_block(&delivered),
            "hi"
        );

        run.resume = true;
        let resumed = run.materialize(&at);
        assert_eq!(resumed.skill.as_ref().unwrap().section, None);
        assert_eq!(resumed.first_input_block(), None, "{driver:?}/{kind:?}");
    }
}

/// No `skillContext` (an older Hub, or a launch outside one) writes and
/// records nothing; kinds `terminal` and `generic` never get anything.
#[test]
fn without_skill_context_or_an_agent_kind_nothing_happens() {
    let at = launch(false);
    let recipe = Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, None).materialize(&at);
    assert!(recipe.skill.is_none());
    assert!(!at.home.join("skills").exists());
    assert!(pairs(&recipe.argv).is_empty());

    let context = facts(&["dispatch", "land"]);
    for (driver, kind) in [
        (DriverKind::ShellPty, AgentKind::Terminal),
        (DriverKind::ShellPty, AgentKind::Generic),
        (DriverKind::GenericPty, AgentKind::Generic),
    ] {
        let at = launch(false);
        let mut spec_run = Run::new(driver, kind, Some(&context));
        spec_run.managed = true;
        let recipe = spec_run.materialize(&at);
        assert!(recipe.skill.is_none(), "{driver:?}/{kind:?}");
        assert_eq!(recipe.first_input_block(), None);
        assert!(!at.home.join("skills").exists());
    }
}

/// The recipe records the decision without the section's bytes leaking
/// into the audit argv when it rides the pair.
#[test]
fn the_recipe_round_trips_the_skill_record() {
    let context = facts(&["dispatch"]);
    let at = launch(true);
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    let json = serde_json::to_value(&recipe).unwrap();
    assert_eq!(json["skill"]["tree"]["state"], "installed");
    assert_eq!(json["skill"]["section"]["mechanism"], "flag");
    assert_eq!(
        json["skill"]["context"]["grants"],
        serde_json::json!(["dispatch"])
    );
    let back: LaunchRecipe = serde_json::from_value(json).unwrap();
    assert_eq!(back.skill, recipe.skill);
    assert_eq!(back.system_prompt, recipe.system_prompt);
}

// ── the capability section ────────────────────────────────────────────────

fn section(grants: &[&str], herdr: bool, delegated: bool) -> String {
    render_section(&SkillContext {
        grants: grants.iter().map(|grant| (*grant).to_owned()).collect(),
        herdr,
        delegated_decisions: delegated,
        inbox: false,
    })
}

fn every_context() -> Vec<SkillContext> {
    let mut out = Vec::new();
    for grants in [
        vec![],
        vec!["dispatch"],
        vec!["land"],
        vec!["dispatch", "land"],
        vec!["address-owner", "dispatch", "land", "spend"],
    ] {
        for herdr in [false, true] {
            for delegated_decisions in [false, true] {
                for inbox in [false, true] {
                    out.push(SkillContext {
                        grants: grants.iter().map(|grant| (*grant).to_owned()).collect(),
                        herdr,
                        delegated_decisions,
                        inbox,
                    });
                }
            }
        }
    }
    out
}

fn mentions_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .any(|token| token.eq_ignore_ascii_case(word))
}

/// Without `land` the land verb and its paragraph are gone entirely — not
/// even the word — and the seat is told not to advance `main` itself.
#[test]
fn land_versus_no_land() {
    let with = section(&["dispatch", "land"], true, true);
    assert!(with.contains("`remuda land`"), "{with}");
    assert!(with.contains("land job"));
    for grants in [&[][..], &["dispatch"][..]] {
        let without = section(grants, true, true);
        assert!(!mentions_word(&without, "land"), "{without}");
        assert!(!without.contains("remuda merge"));
        assert!(without.contains("Never merge into `main`"), "{without}");
    }
    for context in every_context() {
        let text = render_section(&context);
        assert_eq!(
            mentions_word(&text, "land"),
            context.holds("land"),
            "{context:?}\n{text}"
        );
    }
}

/// The worker verbs appear only with `dispatch`.
#[test]
fn dispatch_versus_none() {
    const WORKER_VERBS: &[&str] = &[
        "`remuda dispatch`",
        "`remuda worker ",
        "`remuda retire`",
        "`remuda brief send`",
        "`remuda watch`",
        "`remuda report",
        "`remuda gate ",
        "`remuda task add`",
        "`remuda own claim`",
        "`remuda hostcap",
    ];
    let with = section(&["dispatch"], true, false);
    for verb in WORKER_VERBS {
        assert!(with.contains(verb), "{verb} missing:\n{with}");
    }
    let without = section(&[], true, false);
    for verb in WORKER_VERBS {
        assert!(!without.contains(verb), "{verb} present:\n{without}");
    }
    assert!(without.contains("no `dispatch` grant"));
    assert!(with.contains("Grants: `dispatch`."));
    assert!(without.contains("Grants: none."));
}

/// herdr changes what a same-host dispatch costs, and is said only to a seat
/// that can dispatch.
#[test]
fn herdr_versus_none() {
    let herdr = section(&["dispatch"], true, false);
    let none = section(&["dispatch"], false, false);
    assert!(herdr.contains("advertises herdr"), "{herdr}");
    assert!(herdr.contains("`--carrier herdr`"));
    assert!(none.contains("advertises no herdr"), "{none}");
    assert!(none.contains("asks the owner once"));
    assert!(!none.contains("--carrier herdr"));
    assert_ne!(herdr, none);
    assert!(!section(&[], true, false).contains("herdr"));
}

/// D-051 on routes questions and plan reviews to the seat (never
/// approvals); off sends them to the owner.
#[test]
fn delegated_decisions_on_versus_off() {
    let on = section(&["dispatch"], true, true);
    let off = section(&["dispatch"], true, false);
    assert!(on.contains("Delegated decisions are on"), "{on}");
    assert!(on.contains("`remuda instance respond`"));
    assert!(on.contains("Approvals never come to you"));
    assert!(off.contains("Delegated decisions are off"), "{off}");
    assert!(!off.contains("`remuda instance respond`"));
}

/// Today there is no inbox, and the section says how to watch instead.
#[test]
fn no_inbox_today() {
    let text = section(&["dispatch"], true, false);
    assert!(text.contains("No inbox"), "{text}");
}

/// Every seat is told where its durable memory lives and that the harness's
/// native auto-memory is not it, whatever its grants.
#[test]
fn every_section_carries_the_memory_rule() {
    for context in every_context() {
        let text = render_section(&context);
        assert!(text.contains("Hub's lineage memory"), "{text}");
        assert!(text.contains("Never write the harness's native"), "{text}");
    }
}

/// A `generic-pty` Claude pane IS pinned to its native home once that home
/// holds a login (`generic_pty::pane_home_env`), so Claude reads
/// `<native_home>/skills` there and the tree is installed under the shared-home
/// rules (the home outlives the launch; the section stays out of it and rides the
/// first-input data block).
#[test]
fn a_generic_pty_claude_with_a_logged_in_home_gets_the_tree() {
    let context = facts(&["dispatch"]);
    let at = launch(true);
    // Login evidence, the same test `pane_home_env` applies.
    fs::write(
        at.home.join(".claude.json"),
        r#"{"oauthAccount": {"emailAddress": "seat@example.invalid"}}"#,
    )
    .unwrap();
    let recipe =
        Run::new(DriverKind::GenericPty, AgentKind::Claude, Some(&context)).materialize(&at);
    let root = at.home.join(SKILL_ROOT);
    let skill = recipe.skill.as_ref().unwrap();
    assert_eq!(
        skill.tree,
        SkillTree::Installed {
            root: root.to_string_lossy().into_owned(),
            shared: true
        }
    );
    assert_eq!(
        fs::read(root.join("SKILL.md")).unwrap(),
        embedded_skill_md()
    );
    assert_eq!(
        skill.section,
        Some(SectionDelivery::DataBlock {
            text: skill.data_block().unwrap().to_owned()
        })
    );
    // Never a flag on a raw pane.
    assert!(pairs(&recipe.argv).is_empty(), "{:?}", recipe.argv);
    assert!(
        recipe
            .materialized_files
            .iter()
            .filter(|file| file.role == FileRole::RemudaSkill)
            .all(|file| file.lifetime == FileLifetime::NativeStore)
    );

    // Without the login the pane runs on the operator's own home instead.
    let at = launch(true);
    let plain =
        Run::new(DriverKind::GenericPty, AgentKind::Claude, Some(&context)).materialize(&at);
    assert!(!at.home.join("skills").exists());
    assert_eq!(
        plain.skill.unwrap().tree,
        SkillTree::Skipped {
            reason: TreeSkip::NoSkillReader
        }
    );
}

/// Two seats launched together on one shared home write the same files: every
/// file lands whole (no interleaved or truncated bytes) and no temporary file
/// survives. Before the temp name carried a per-writer serial, both launches
/// inside one Node process picked the same name.
#[test]
fn concurrent_installs_on_one_shared_home_never_clobber_each_other() {
    let context = facts(&["dispatch", "land"]);
    let at = launch(true);
    let expected: Vec<(&str, Vec<u8>)> = embedded_skill_files()
        .map(|(rel, bytes)| (rel, bytes.to_vec()))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let context = &context;
            let at = &at;
            scope.spawn(move || {
                for _ in 0..25 {
                    let recipe = Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(context))
                        .materialize(at);
                    let skill = recipe.skill.unwrap();
                    assert!(
                        matches!(skill.tree, SkillTree::Installed { shared: true, .. }),
                        "{:?}",
                        skill.tree
                    );
                }
            });
        }
    });
    let root = at.home.join(SKILL_ROOT);
    for (rel, bytes) in expected {
        assert_eq!(fs::read(root.join(rel)).unwrap(), bytes, "{rel}");
    }
    let leftovers: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp") || name.starts_with('.'))
        .filter(|name| name != MANAGED_MARKER)
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

/// A write that fails after its temporary file exists removes the temporary:
/// a leftover dotfile would make a later launch read the tree as non-empty and
/// therefore user-owned.
#[test]
fn a_failed_write_leaves_no_temporary_file_behind() {
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let root = at.home.join(SKILL_ROOT);
    fs::create_dir_all(&root).unwrap();
    // Remuda's own tree, but `SKILL.md` is a directory, so the rename onto it
    // fails after the temporary file was written.
    fs::write(root.join(MANAGED_MARKER), b"remuda").unwrap();
    fs::create_dir_all(root.join("SKILL.md")).unwrap();
    let mut spec = spec(DriverKind::ClaudeSdk, AgentKind::Claude);
    spec.capabilities.clear();
    let profile = profile(DriverKind::ClaudeSdk);
    let result = materialize(&MaterializeRequest {
        creator: remuda_driver::CreatorControl::default(),
        spec: &spec,
        profile: &profile,
        launch_dir: at.launch.clone(),
        native_home: at.home.clone(),
        session: SessionAction::New {
            session_id: "01993ab0-0000-7000-8000-000000000003".into(),
        },
        launch_id: Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(remuda_driver::BinaryPin {
            abs_path: at.binary.to_string_lossy().into_owned(),
            version: "2.1.291 (Claude Code)".into(),
            sha256: hash_file(&at.binary).unwrap(),
        }),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: Some(true),
        settings_overlay_path: None,
        secret_policy: None,
        memory_context: None,
        skill_context: Some(&context),
    });
    assert!(
        result.is_err(),
        "the rename onto a directory must fail the launch"
    );
    let leftovers: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

/// The marker is accepted by type, not by name: a `.remuda-managed` that is a
/// link is not Remuda's, and writing through it would truncate the file it
/// points at.
#[cfg(unix)]
#[test]
fn a_symlinked_marker_makes_the_tree_the_users() {
    use std::os::unix::fs::symlink;
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let root = at.home.join(SKILL_ROOT);
    fs::create_dir_all(&root).unwrap();
    let outside = at._root.path().join("outside.txt");
    fs::write(&outside, b"the user's file\n").unwrap();
    symlink(&outside, root.join(MANAGED_MARKER)).unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(
        recipe.skill.as_ref().unwrap().tree,
        SkillTree::Skipped {
            reason: TreeSkip::UserOwned
        }
    );
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"the user's file\n",
        "the target is untouched"
    );
    assert!(
        fs::symlink_metadata(root.join(MANAGED_MARKER))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the marker link is untouched"
    );
    assert!(!root.join("SKILL.md").exists(), "no tree was written");
}

/// A dangling marker link is not a marker either: following it would create a
/// file outside the home.
#[cfg(unix)]
#[test]
fn a_dangling_marker_link_makes_the_tree_the_users() {
    use std::os::unix::fs::symlink;
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let root = at.home.join(SKILL_ROOT);
    fs::create_dir_all(&root).unwrap();
    let target = at._root.path().join("nowhere.txt");
    symlink(&target, root.join(MANAGED_MARKER)).unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(
        recipe.skill.as_ref().unwrap().tree,
        SkillTree::Skipped {
            reason: TreeSkip::UserOwned
        }
    );
    assert!(!target.exists(), "nothing was created through the link");
    assert!(
        fs::symlink_metadata(root.join(MANAGED_MARKER))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// Cleanup unlinks in recipe order, so the marker is recorded last: a scan
/// running during cleanup never sees `SKILL.md` without the marker and takes
/// the tree for the user's.
#[test]
fn the_marker_is_cleaned_up_last() {
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    let tree: Vec<&str> = recipe
        .materialized_files
        .iter()
        .filter(|file| file.role == FileRole::RemudaSkill)
        .filter(|file| file.lifetime == FileLifetime::Launch)
        .map(|file| file.path.as_str())
        .collect();
    assert!(tree.len() >= 2, "{tree:?}");
    assert!(
        tree.last().unwrap().ends_with(MANAGED_MARKER),
        "the marker must be removed last: {tree:?}"
    );
    assert!(
        tree[..tree.len() - 1]
            .iter()
            .any(|path| path.ends_with("SKILL.md"))
    );
    // And cleanup really takes every one of them.
    recipe.cleanup_launch_files();
    for path in tree {
        assert!(!Path::new(path).exists(), "{path} survived cleanup");
    }
}

/// The `skills` directory is often the user's own: its mode is theirs. Only
/// directories Remuda creates get 0700.
#[cfg(unix)]
#[test]
fn a_user_skills_directory_keeps_its_mode() {
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let skills = at.home.join("skills");
    fs::create_dir_all(&skills).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&skills, fs::Permissions::from_mode(0o751)).unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert!(matches!(
        recipe.skill.unwrap().tree,
        SkillTree::Installed { shared: false, .. }
    ));
    let mode = fs::metadata(&skills).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o751,
        "a directory Remuda did not create keeps its mode"
    );
    // The tree Remuda did create is owner-only.
    let root = skills.join("remuda");
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// A symlink is never followed: `skills/remuda` pointing at an empty
/// directory is still the user's (Remuda did not create that target), so the
/// launch skips the tree and the target stays empty.
#[cfg(unix)]
#[test]
fn a_symlinked_skill_root_is_user_owned_and_never_followed() {
    use std::os::unix::fs::symlink;
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let target = at._root.path().join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let skills = at.home.join("skills");
    fs::create_dir_all(&skills).unwrap();
    let root = skills.join("remuda");
    symlink(&target, &root).unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(
        recipe.skill.as_ref().unwrap().tree,
        SkillTree::Skipped {
            reason: TreeSkip::UserOwned
        }
    );
    assert!(
        fs::read_dir(&target).unwrap().next().is_none(),
        "the link target stays empty"
    );
    assert!(
        fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is untouched"
    );
    // The section still reaches the seat, on the launch's own pair.
    assert!(recipe.skill.unwrap().flag_text().is_some());
}

/// The same for a symlinked `skills` parent: writing through it would put the
/// tree outside the home.
#[cfg(unix)]
#[test]
fn a_symlinked_skills_parent_is_user_owned_and_never_followed() {
    use std::os::unix::fs::symlink;
    let context = facts(&["dispatch"]);
    let at = launch(false);
    let target = at._root.path().join("elsewhere");
    fs::create_dir_all(&target).unwrap();
    let skills = at.home.join("skills");
    symlink(&target, &skills).unwrap();
    let recipe =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(
        recipe.skill.as_ref().unwrap().tree,
        SkillTree::Skipped {
            reason: TreeSkip::UserOwned
        }
    );
    assert!(
        fs::read_dir(&target).unwrap().next().is_none(),
        "the link target stays empty"
    );
    assert!(
        fs::symlink_metadata(&skills)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

// ── the parity ledger ──────────────────────────────────────────────────────

/// The CLI subcommands the ledger lists, split by what its Today column says.
///
/// `agent`: an Agent-origin caller gets the subcommand (`Agent route …`).
/// `other`: every other row — Safety, Gap, Local, Deployment, not an agent
/// action. The section may promise only `agent` verbs: `remuda merge`, for
/// one, is `Local` and bypasses the Hub's `land` grant, so a one-word match
/// against any Today row must never accept it.
fn ledger() -> (BTreeSet<String>, BTreeSet<String>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/agent-parity.md");
    let text = fs::read_to_string(&path).unwrap();
    let start = text
        .find("\n## CLI subcommands")
        .expect("the ledger has a CLI section");
    let rest = &text[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |at| at + 3);
    let mut agent = BTreeSet::new();
    let mut other = BTreeSet::new();
    for line in rest[..end].lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // `| subcommand | route or kind | Today | Planned |`
        if cells.len() < 6 || !cells[1].starts_with('`') {
            continue;
        }
        let name = cells[1]
            .trim_start_matches('`')
            .split('`')
            .next()
            .unwrap()
            .split_whitespace()
            .take_while(|word| !word.starts_with('<') && !word.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");
        if cells[3].starts_with("Agent route") {
            agent.insert(name);
        } else {
            other.insert(name);
        }
    }
    (agent, other)
}

/// Every `remuda …` command the section can render, for every combination of
/// facts, resolves to a subcommand the ledger's Today column gives an Agent:
/// the longest of its words that names a row (at most three) must be that
/// row, so neither a Gap/Safety/Local verb nor a shorter prefix that happens
/// to be one slips through.
#[test]
fn the_section_names_only_verbs_in_the_ledger_today_column() {
    let (agent, other) = ledger();
    assert!(agent.contains("instance read") && agent.contains("land"));
    assert!(other.contains("instance list") && other.contains("instance keys"));
    // The traps this test exists for.
    assert!(other.contains("merge") && other.contains("host files list"));
    assert!(!agent.contains("merge"));
    let mut named = BTreeSet::new();
    for context in every_context() {
        let text = render_section(&context);
        for span in text.split('`').skip(1).step_by(2) {
            let Some(command) = span.strip_prefix("remuda ") else {
                continue;
            };
            let words: Vec<&str> = command
                .split_whitespace()
                .take_while(|word| !word.starts_with('<') && !word.starts_with('-'))
                .collect();
            let resolved = (1..=words.len().min(3))
                .rev()
                .map(|len| words[..len].join(" "))
                .find(|verb| agent.contains(verb) || other.contains(verb))
                .unwrap_or_default();
            assert!(
                agent.contains(&resolved),
                "`remuda {command}` resolves to `{resolved}`, which the ledger's Today column \
                 does not give an Agent ({context:?})"
            );
            named.insert(resolved);
        }
    }
    // The walk saw the verbs it is meant to police.
    for verb in [
        "dispatch",
        "land",
        "worker nudge",
        "instance respond",
        "gate",
    ] {
        assert!(named.contains(verb), "{verb} never rendered: {named:?}");
    }
}

/// The section rides as data with the permission mode untouched.
#[test]
fn the_section_does_not_change_the_permission_mode() {
    let context = facts(&["dispatch", "land"]);
    let at = launch(true);
    let plain = Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, None).materialize(&at);
    let stamped =
        Run::new(DriverKind::ClaudeSdk, AgentKind::Claude, Some(&context)).materialize(&at);
    assert_eq!(plain.permission, stamped.permission);
}

// ── a real carrier start ───────────────────────────────────────────────────

/// The claude-sdk carrier, started for real against the fake `claude`: on a
/// per-instance home the tree is in place before the child runs and argv has
/// no pair for it; on the inherited home the child receives the section as
/// the materializer's own pair, which the finished-argv guard lets through.
#[tokio::test]
async fn a_real_claude_sdk_start_carries_the_skill() {
    use remuda_driver::Driver;
    use remuda_driver::claude_sdk::{ClaudeSdkDriver, ClaudeSdkOptions};
    use std::collections::BTreeMap;
    use std::time::Duration;

    let fake = remuda_testing::ensure_workspace_bin("fake-claude")
        .canonicalize()
        .unwrap();
    let pin = remuda_driver::BinaryPin {
        abs_path: fake.to_string_lossy().into_owned(),
        version: "2.1.291 (Claude Code)".into(),
        sha256: remuda_protocol::Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap(),
    };
    let context = facts(&["dispatch"]);
    for inherited in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let instance = tmp.path().join("instances").join("ins_x");
        let launch_dir = instance.join("launch");
        let home = instance.join("native-home");
        fs::create_dir_all(&launch_dir).unwrap();
        fs::create_dir_all(&home).unwrap();
        let recorded = tmp.path().join("argv.txt");
        let mut env = BTreeMap::new();
        env.insert(
            "FAKE_CLAUDE_SCRIPT".to_owned(),
            remuda_testing::script_path(remuda_testing::ScriptKind::Ok)
                .to_string_lossy()
                .into_owned(),
        );
        env.insert(
            "FAKE_CLAUDE_ARGV_FILE".to_owned(),
            recorded.to_string_lossy().into_owned(),
        );
        let mut options = ClaudeSdkOptions::new(
            profile(DriverKind::ClaudeSdk),
            launch_dir,
            home.clone(),
            BinarySource::Pinned(pin.clone()),
        );
        options.origin = remuda_protocol::InputOrigin::Human;
        options.extra_env = env;
        options.handshake_timeout = Duration::from_secs(5);
        options.close_timeout = Duration::from_secs(3);
        options.inherit_default_config = inherited;
        options.skill_context = Some(context.clone());
        let mut spec = spec(DriverKind::ClaudeSdk, AgentKind::Claude);
        spec.cwd = tmp
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let driver = ClaudeSdkDriver::new(options);
        driver
            .start(spec)
            .await
            .unwrap_or_else(|error| panic!("inherited={inherited}: {error}"));
        let argv: Vec<String> = fs::read_to_string(&recorded)
            .expect("the fake recorded its argv")
            .lines()
            .map(str::to_owned)
            .collect();
        let at = pairs(&argv);
        let installed = home.join(SKILL_ROOT).join("SKILL.md");
        if inherited {
            assert!(
                !home.join("skills").exists(),
                "the operator home is untouched"
            );
            assert_eq!(at.len(), 1, "{argv:?}");
            // The fake writes one line per line of argv, so the multi-line
            // value continues past its first line.
            let value = argv[at[0] + 1..].join("\n");
            assert!(value.starts_with(OPEN), "{argv:?}");
            assert!(value.contains("`remuda dispatch`"), "{value}");
        } else {
            assert!(at.is_empty(), "{argv:?}");
            let text = fs::read_to_string(&installed).unwrap();
            assert!(text.ends_with(&render_section(&context)), "{text}");
        }
        driver.close().await.expect("close");
    }
}
