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

// ── the parity ledger ──────────────────────────────────────────────────────

/// The CLI subcommands the ledger lists, split by whether the Today column
/// says an Agent can use them (`Agent route …` or `Local`).
fn ledger() -> (BTreeSet<String>, BTreeSet<String>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/agent-parity.md");
    let text = fs::read_to_string(&path).unwrap();
    let start = text
        .find("\n## CLI subcommands")
        .expect("the ledger has a CLI section");
    let rest = &text[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |at| at + 3);
    let mut today = BTreeSet::new();
    let mut not_today = BTreeSet::new();
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
        let available = cells[3].starts_with("Agent route") || cells[3].starts_with("Local");
        if available {
            today.insert(name);
        } else {
            not_today.insert(name);
        }
    }
    (today, not_today)
}

/// Every `remuda …` verb the section can render, for every combination of
/// facts, is a subcommand the ledger's Today column gives an Agent; the
/// section names no Gap, Safety or deployment row.
#[test]
fn the_section_names_only_verbs_in_the_ledger_today_column() {
    let (today, not_today) = ledger();
    assert!(today.contains("instance read") && today.contains("land"));
    assert!(not_today.contains("instance list") && not_today.contains("instance keys"));
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
            let two = words.iter().take(2).copied().collect::<Vec<_>>().join(" ");
            let one = words.first().copied().unwrap_or_default().to_owned();
            let verb = if today.contains(&two) || not_today.contains(&two) {
                two
            } else {
                one
            };
            assert!(
                today.contains(&verb),
                "`remuda {command}` is not in the ledger's Today column ({context:?})"
            );
            named.insert(verb);
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
