//! D-045: the `computer-use` per-launch capability.
//!
//! These pin the whole delivery contract at the materializer boundary:
//! nothing is written unless granted; the embedded skill matches the shipped
//! directory byte-for-byte; a managed claude home gets the skill tree while an
//! inherited operator home is never touched; claude mounts via `--mcp-config`,
//! codex via the shadow config table; every refusal names the value; and the
//! handshake env rides only a granted recipe.

use remuda_driver::{
    BinarySource, Delegation, DriverError, FileLifetime, FileRole, LaunchOrigin, LaunchRecipe,
    MaterializeRequest, ProviderHealth, ProviderKind, ProviderProfile, SessionAction, hash_file,
    materialize,
};
use remuda_protocol::{
    AgentKind, ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, DriverKind, Id,
    InstanceSpec, PermissionMode,
};
use remuda_testing::install_executable;
use sha2::Digest as _;
use std::fs;
use std::path::{Path, PathBuf};

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

fn load_spec() -> InstanceSpec {
    serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap()
}

fn stub_binary(dir: &Path) -> PathBuf {
    install_executable(dir, "claude", "#!/bin/sh\necho '2.1.268 (Claude Code)'\n")
}

struct Dirs {
    root: tempfile::TempDir,
    launch: PathBuf,
    home: PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let launch = root.path().join("launch");
    let home = root.path().join("native-home");
    fs::create_dir_all(&launch).unwrap();
    fs::create_dir_all(&home).unwrap();
    Dirs { root, launch, home }
}

fn request<'a>(
    spec: &'a mut InstanceSpec,
    launch: &'a Path,
    home: &'a Path,
    managed: bool,
    binary: &'a Path,
) -> MaterializeRequest<'a> {
    MaterializeRequest {
        spec,
        profile: Box::leak(Box::new(profile())),
        launch_dir: launch.to_path_buf(),
        native_home: home.to_path_buf(),
        session: SessionAction::New {
            session_id: "01993ab0-0000-7000-8000-000000000003".into(),
        },
        launch_id: Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(BinaryPinShim::pin(binary)),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: Some(managed),
        settings_overlay_path: None,
        secret_policy: None,
    }
}

/// Tiny indirection so the test file does not repeat the pin boilerplate.
struct BinaryPinShim;
impl BinaryPinShim {
    fn pin(path: &Path) -> remuda_driver::BinaryPin {
        let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        remuda_driver::BinaryPin {
            abs_path: abs.to_string_lossy().into_owned(),
            version: "stub".into(),
            sha256: hash_file(&abs).expect("hash stub"),
        }
    }
}

fn grant(spec: &mut InstanceSpec) {
    spec.capabilities = vec!["computer-use".to_owned()];
}

const ENV_NAME: &str = "REMUDA_CAPABILITY_COMPUTER_USE";
const SKILL_ROOT: &str = "skills/codex-computer-use";

const EMBEDDED_FILES: &[&str] = &[
    "SKILL.md",
    "references/setup.md",
    "scripts/launch-cua-repl.sh",
    "scripts/launch-mcp.sh",
    "scripts/probe_mcp.py",
];

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

fn digest_pairs(pairs: &[(String, Vec<u8>)]) -> String {
    let mut hasher = sha2::Sha256::new();
    for (rel, bytes) in pairs {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

#[test]
fn embedded_skill_matches_the_shipped_directory_byte_for_byte() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/codex-computer-use");
    let on_disk = walk_files(&source);

    // Same file set, same relative paths.
    let disk_names: Vec<&str> = on_disk.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        disk_names, EMBEDDED_FILES,
        "EMBEDDED_SKILL must list every shipped file"
    );

    // Same aggregate digest the embedded set is compared against.
    let embedded_refs: Vec<(&str, &[u8])> = on_disk
        .iter()
        .map(|(rel, _)| {
            // map through embedded_skill_files so we read embedded bytes
            (rel.as_str(), ())
        })
        .map(|(rel, ())| {
            let bytes = remuda_driver::launch::skills::embedded_skill_files()
                .find(|(name, _)| *name == rel)
                .unwrap_or_else(|| panic!("embedded copy missing {rel}"))
                .1;
            (rel, bytes)
        })
        .collect();
    assert_eq!(
        remuda_driver::launch::skills::skill_tree_digest(embedded_refs),
        digest_pairs(&on_disk)
    );
}

// ── granted claude, managed home: full delivery ───────────────────────────

#[test]
fn granted_claude_writes_skill_tree_mcp_config_argv_and_env() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    grant(&mut spec);

    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();

    // Leg (a): every embedded file is in the managed home, byte-identical and
    // owner-read-only.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/codex-computer-use");
    for rel in EMBEDDED_FILES {
        let delivered = dirs.home.join(SKILL_ROOT).join(rel);
        assert!(delivered.is_file(), "missing {rel}");
        assert_eq!(
            fs::read(&delivered).unwrap(),
            fs::read(source.join(rel)).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&delivered).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{rel} mode {mode:o}");
        }
    }

    // Every directory in the delivered tree is owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let tree_root = dirs.home.join("skills").join("codex-computer-use");
        let _ = tree_root.clone();
        let mut stack = vec![tree_root.clone()];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "dir {} mode {mode:o}", dir.display());
            checked += 1;
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        assert!(checked >= 2, "root + references/scripts dirs");
    }

    // Leg (b): 0600 per-instance config + 0755 launchers under launch/.
    let mcp_config = dirs.launch.join("mcp-cua.json");
    let repl = dirs.launch.join("cua/scripts/launch-cua-repl.sh");
    let mcp_sh = dirs.launch.join("cua/scripts/launch-mcp.sh");
    assert!(mcp_config.is_file() && repl.is_file() && mcp_sh.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&mcp_config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&repl).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&mcp_sh).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(&mcp_config).unwrap()).unwrap();
    let server = &config["mcpServers"]["codex-computer-use"];
    assert_eq!(server["command"], "/bin/sh");
    assert_eq!(server["args"][0], repl.to_string_lossy().as_ref());
    assert_eq!(server["env"][ENV_NAME], "1");

    // argv mounts the config exactly once, never --strict-mcp-config.
    assert_eq!(
        recipe
            .argv
            .iter()
            .filter(|token| token.as_str() == "--mcp-config")
            .count(),
        1
    );
    assert!(
        recipe
            .argv
            .windows(2)
            .any(|pair| pair[0] == "--mcp-config" && pair[1] == mcp_config.to_string_lossy())
    );
    assert!(!recipe.argv.iter().any(|t| t == "--strict-mcp-config"));

    // Env handshake on the allowlist, audited.
    assert!(
        recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == ENV_NAME
                && entry.source == remuda_driver::EnvAllowlistSource::Capability)
    );
    assert_eq!(recipe.capabilities, vec!["computer-use"]);
    assert_eq!(recipe.mcp_servers.len(), 1);

    // Audit: every written file has a digest and Launch lifetime.
    let roles: Vec<FileRole> = recipe
        .materialized_files
        .iter()
        .map(|file| file.role)
        .collect();
    assert!(roles.contains(&FileRole::CapabilityMcpConfig));
    assert!(roles.contains(&FileRole::CapabilityScript));
    assert!(
        roles
            .iter()
            .filter(|role| **role == FileRole::CapabilitySkill)
            .count()
            == 5
    );
    for file in &recipe.materialized_files {
        assert!(!String::from(file.content_digest.clone()).is_empty());
        assert_eq!(file.lifetime, FileLifetime::Launch);
    }
}

#[test]
fn granted_skill_tree_in_a_shared_home_is_native_store_lifetime() {
    // A configured shared dir lives OUTSIDE the per-instance launch dir, so its
    // skill files must survive one instance's cleanup (review 6).
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("instances/i/launch");
    let shared_home = tmp.path().join("shared-claude-home");
    fs::create_dir_all(&launch).unwrap();
    fs::create_dir_all(&shared_home).unwrap();
    let binary = stub_binary(tmp.path());
    let mut spec = load_spec();
    grant(&mut spec);

    let recipe = materialize(&MaterializeRequest {
        spec: &spec,
        profile: Box::leak(Box::new(profile())),
        launch_dir: launch.clone(),
        native_home: shared_home.clone(),
        session: SessionAction::New {
            session_id: "01993ab0-0000-7000-8000-000000000003".into(),
        },
        launch_id: Id::new("launch").unwrap(),
        binary: BinarySource::Pinned(BinaryPinShim::pin(&binary)),
        setting_sources: None,
        origin: LaunchOrigin::Human,
        native_home_managed: Some(true),
        settings_overlay_path: None,
        secret_policy: None,
    })
    .unwrap();

    let skill = recipe
        .materialized_files
        .iter()
        .find(|file| file.role == FileRole::CapabilitySkill)
        .expect("skill file recorded");
    assert_eq!(
        skill.lifetime,
        FileLifetime::NativeStore,
        "shared-home skill files must not be shredded on instance exit"
    );
    assert!(shared_home.join(SKILL_ROOT).join("SKILL.md").is_file());

    // Launch cleanup leaves the shared-home tree intact.
    for (path, error) in recipe.cleanup_launch_files() {
        assert!(error.is_none(), "{path}");
    }
    assert!(shared_home.join(SKILL_ROOT).join("SKILL.md").is_file());
}

#[test]
fn granted_claude_with_an_inherited_home_writes_nothing_into_it() {
    // Simulate the operator home: a marker the delivery must never touch.
    let dirs = dirs();
    let marker = dirs.home.join(".operator-config");
    fs::write(&marker, b"mine").unwrap();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    grant(&mut spec);

    let recipe = materialize(&request(
        &mut spec,
        &dirs.launch,
        &dirs.home,
        false, // inherited operator ~/.claude
        &binary,
    ))
    .unwrap();

    // No skills tree under the operator home; its contents are unchanged.
    assert!(!dirs.home.join(SKILL_ROOT).exists());
    assert_eq!(fs::read(&marker).unwrap(), b"mine");

    // Leg (b) still happens — it only touches the instance launch dir.
    assert!(dirs.launch.join("mcp-cua.json").is_file());
    assert_eq!(
        recipe
            .materialized_files
            .iter()
            .filter(|file| file.role == FileRole::CapabilitySkill)
            .count(),
        0
    );
    assert!(
        recipe.argv.windows(2).any(|pair| pair[0] == "--mcp-config"),
        "the MCP config is still delivered via argv"
    );
}

#[test]
fn ungranted_launch_writes_nothing_and_sets_no_handshake() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec(); // no capabilities

    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();
    assert!(!dirs.launch.join("mcp-cua.json").exists());
    assert!(!dirs.launch.join("cua").exists());
    assert!(!dirs.home.join(SKILL_ROOT).exists());
    assert!(recipe.capabilities.is_empty());
    assert!(recipe.mcp_servers.is_empty());
    assert!(!recipe.argv.iter().any(|token| token == "--mcp-config"));
    assert!(
        !recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == ENV_NAME)
    );
}

// ── codex: shadow config, never argv ──────────────────────────────────────

#[test]
fn granted_codex_delivers_only_the_mcp_server_record_for_the_shadow_home() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    spec.kind = AgentKind::Codex;
    spec.driver = DriverKind::ShellPty;
    grant(&mut spec);

    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();

    // No argv mount: codex reads the shadow config.toml.
    assert!(!recipe.argv.iter().any(|token| token == "--mcp-config"));
    // The launchers/config are still written under the launch dir.
    assert!(dirs.launch.join("mcp-cua.json").is_file());
    assert_eq!(recipe.capabilities, vec!["computer-use"]);
    assert_eq!(recipe.mcp_servers.len(), 1);
    let server = &recipe.mcp_servers[0];
    assert_eq!(server.name, "codex-computer-use");
    assert!(
        server
            .env
            .iter()
            .any(|(name, value)| name == ENV_NAME && value == "1")
    );
    // The server entry carries the operator's REAL codex home (launcher-only),
    // never the shadow home the agent itself gets — the vendor app lives there.
    let real_home = server
        .env
        .iter()
        .find(|(name, _)| name == "REMUDA_CODEX_HOME")
        .map(|(_, value)| value.as_str())
        .expect("MCP server entry must carry the real codex home");
    assert!(
        real_home.ends_with(".codex"),
        "real home should resolve to $HOME/.codex here: {real_home}"
    );
    // The agent-facing handshake allowlist never carries the real home.
    assert!(
        !recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == "REMUDA_CODEX_HOME")
    );

    // The shadow materializer splices the same server into config.toml and
    // keeps [features] / [hooks.state] intact.
    let shadow = remuda_driver::launch::materialize_codex(&remuda_driver::launch::ShadowOptions {
        launch_dir: &dirs.launch.join("shadow-check"),
        relay_binary: Path::new("/opt/remuda/bin/remuda"),
        socket_path: &dirs.root.path().join("hook.sock"),
        mcp_servers: &[remuda_driver::launch::ShadowMcpServer {
            name: server.name.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            env: server.env.clone(),
        }],
    })
    .unwrap();
    let toml_text = fs::read_to_string(shadow.home.join("config.toml")).unwrap();
    let parsed: toml::Value = toml_text.parse().unwrap();
    assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
    assert_eq!(
        parsed["mcp_servers"]["codex-computer-use"]["command"].as_str(),
        Some("/bin/sh")
    );
    assert_eq!(
        parsed["mcp_servers"]["codex-computer-use"]["env"][ENV_NAME].as_str(),
        Some("1")
    );
    assert_eq!(
        parsed["mcp_servers"]["codex-computer-use"]["env"]["REMUDA_CODEX_HOME"]
            .as_str()
            .map(|value| value.ends_with(".codex")),
        Some(true)
    );
}

#[test]
fn granted_codex_on_a_non_shell_carrier_writes_its_own_shadow_config() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    spec.kind = AgentKind::Codex;
    // generic-pty, unlike shell-pty, has no HookSession: the materializer must
    // deliver the codex `config.toml` itself (D-045 §3.3).
    spec.driver = DriverKind::GenericPty;
    grant(&mut spec);

    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();

    let config = dirs.launch.join("codex-home/config.toml");
    assert!(config.is_file(), "codex shadow config must be materialized");
    let parsed: toml::Value = fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(
        parsed["mcp_servers"]["codex-computer-use"]["command"].as_str(),
        Some("/bin/sh")
    );
    assert_eq!(
        parsed["mcp_servers"]["codex-computer-use"]["env"][ENV_NAME].as_str(),
        Some("1")
    );
    assert!(
        recipe
            .materialized_files
            .iter()
            .any(|file| file.path.ends_with("codex-home/config.toml")
                && file.role == FileRole::CapabilityMcpConfig),
        "the shadow config is in the launch audit"
    );
    // The shadow home must be pinned as CODEX_HOME so the codex binary reads
    // that config (generic-pty has no hook session to do it for it).
    assert!(
        recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == "CODEX_HOME"
                && entry.source == remuda_driver::EnvAllowlistSource::NativeHome),
        "CODEX_HOME must pin the shadow home: {:?}",
        recipe.env_allowlist
    );
}

// ── refusals: named, before any file is written ───────────────────────────

fn refuse_case(spec: &mut InstanceSpec, origin: LaunchOrigin, managed: bool, needle: &str) {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut request = request(spec, &dirs.launch, &dirs.home, managed, &binary);
    request.origin = origin;
    let error = materialize(&request).unwrap_err();
    assert!(
        matches!(error, DriverError::InvalidLaunchSpec(_)),
        "{error}"
    );
    assert!(error.to_string().contains(needle), "{error}");
    // Refusal writes nothing.
    assert!(!dirs.launch.join("mcp-cua.json").exists());
    assert!(!dirs.home.join(SKILL_ROOT).exists());
}

#[test]
fn unknown_capability_value_is_refused_and_named() {
    let mut spec = load_spec();
    spec.capabilities = vec!["desktop".to_owned()];
    refuse_case(&mut spec, LaunchOrigin::Human, true, "\"desktop\"");
}

#[test]
fn agent_origin_is_refused() {
    let mut spec = load_spec();
    grant(&mut spec);
    refuse_case(&mut spec, LaunchOrigin::Agent, true, "agent-originated");
}

#[test]
fn bypass_permissions_plus_computer_use_is_refused() {
    let mut spec = load_spec();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::Host,
    }));
    grant(&mut spec);
    refuse_case(
        &mut spec,
        LaunchOrigin::Human,
        true,
        "unattended/skipped tool approvals",
    );
}

#[test]
fn grok_and_other_kinds_are_refused_this_batch() {
    for kind in [
        AgentKind::Grok,
        AgentKind::Agy,
        AgentKind::Generic,
        AgentKind::Terminal,
    ] {
        let mut spec = load_spec();
        spec.kind = kind;
        spec.driver = match kind {
            AgentKind::Grok => DriverKind::ShellPty,
            _ => DriverKind::ShellPty,
        };
        grant(&mut spec);
        refuse_case(&mut spec, LaunchOrigin::Human, true, "not supported");
    }
}

#[test]
fn caller_supplied_mcp_config_collides_with_the_granted_one() {
    for extra in [
        vec!["--mcp-config".to_owned(), "/tmp/other.json".to_owned()],
        vec!["--mcp-config=/tmp/other.json".to_owned()],
    ] {
        let dirs = dirs();
        let binary = stub_binary(dirs.root.path());
        let mut spec = load_spec();
        grant(&mut spec);
        spec.args = extra;
        let error =
            materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap_err();
        assert!(error.to_string().contains("--mcp-config"), "{error}");
        // Refused before any capability file exists.
        assert!(!dirs.launch.join("mcp-cua.json").exists());
        assert!(!dirs.home.join(SKILL_ROOT).exists());
    }
}

#[test]
fn a_capability_sourced_env_entry_with_another_name_is_still_denied() {
    // The deny-prefix hole is the one handshake name, not the source tag:
    // callers can't mint arbitrary REMUDA_ vars by tagging them Capability.
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    grant(&mut spec);
    // Simulate a corrupted/other entry as the spawn sites would filter it.
    let name = "REMUDA_SOMETHING_ELSE";
    assert!(remuda_driver::child_env::is_denied(name));
    assert_ne!(
        name,
        remuda_driver::launch::skills::CAPABILITY_COMPUTER_USE_ENV
    );
    // Sanity: materialize never creates such an entry.
    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();
    let extra = recipe
        .env_allowlist
        .iter()
        .filter(|entry| entry.source == remuda_driver::EnvAllowlistSource::Capability)
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        extra,
        vec![remuda_driver::launch::skills::CAPABILITY_COMPUTER_USE_ENV],
        "only the one handshake name may ride the Capability source"
    );
}

#[test]
fn codex_never_policy_is_refused_even_when_granted() {
    // Q4 is harness-agnostic: codex ApprovalPolicy::Never + computer-use
    // refuses (the old gate matched only Claude).
    let mut spec = load_spec();
    spec.kind = AgentKind::Codex;
    spec.driver = DriverKind::GenericPty;
    spec.permission_mode = PermissionMode::Codex(Box::new(remuda_protocol::CodexPermission {
        approval_policy: remuda_protocol::ApprovalPolicy::Never,
        approvals_reviewer: remuda_protocol::ApprovalsReviewer::User,
        execution: remuda_protocol::CodexExecution::Sandbox(remuda_protocol::SandboxExecution {
            sandbox: remuda_protocol::SandboxMode::WorkspaceWrite,
        }),
    }));
    grant(&mut spec);
    refuse_case(&mut spec, LaunchOrigin::Human, true, "unattended");
}

// ── audit and cleanup ─────────────────────────────────────────────────────

#[test]
fn launch_cleanup_removes_the_capability_files() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    grant(&mut spec);
    let recipe = materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();
    let mcp_config = dirs.launch.join("mcp-cua.json");
    assert!(mcp_config.is_file());

    for (path, error) in recipe.cleanup_launch_files() {
        assert!(error.is_none(), "{path}: {error:?}");
    }
    assert!(!mcp_config.exists());
    assert!(!dirs.launch.join("cua/scripts/launch-cua-repl.sh").exists());
    // Skill files in the managed home are Launch-lifetime too.
    assert!(!dirs.home.join(SKILL_ROOT).join("SKILL.md").exists());
}

#[test]
fn recipe_json_records_the_grant_without_secret_shaped_values() {
    let dirs = dirs();
    let binary = stub_binary(dirs.root.path());
    let mut spec = load_spec();
    grant(&mut spec);
    let recipe: LaunchRecipe =
        materialize(&request(&mut spec, &dirs.launch, &dirs.home, true, &binary)).unwrap();
    let json = serde_json::to_string(&recipe).unwrap();
    assert!(json.contains("computer-use"));
    assert!(json.contains("--mcp-config"));
    // The handshake value is not secret, but it rides as a name on the
    // allowlist, never as an inline env value on the recipe.
    let audit = serde_json::from_str::<LaunchRecipe>(&json).unwrap();
    assert_eq!(audit.capabilities, vec!["computer-use"]);
    assert!(
        audit
            .env_allowlist
            .iter()
            .any(|entry| entry.name == ENV_NAME)
    );
}
