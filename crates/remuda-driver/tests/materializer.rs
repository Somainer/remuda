//! Launch materializer: idempotency, secret omission, banned flags, binary pin.

use remuda_driver::{
    BinarySource, Delegation, DriverError, LaunchRecipe, MaterializeRequest, ProviderHealth,
    ProviderKind, ProviderProfile, SecretRef, SessionAction, TECH_DEBT_M0_PERM_01, materialize,
    pin_binary,
};
use remuda_protocol::{
    ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, EnvBinding, EnvVisibility, Id,
    InstanceSpec, LiteralEnv, PermissionMode,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn load_spec() -> InstanceSpec {
    serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap()
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: "https://gateway.example".into(),
        delegation: Delegation::Astergate,
        secret_ref: SecretRef::parse("env:REMUDA_TEST_SECRET").unwrap(),
        models: vec!["passthrough/example-model".into()],
        health: ProviderHealth::Healthy,
    }
}

fn stub_binary(dir: &Path, version: &str) -> PathBuf {
    let path = dir.join("claude");
    fs::write(&path, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn request<'a>(
    spec: &'a InstanceSpec,
    profile: &'a ProviderProfile,
    launch: &Path,
    home: &Path,
    binary: BinarySource,
) -> MaterializeRequest<'a> {
    MaterializeRequest {
        spec,
        profile,
        launch_dir: launch.to_path_buf(),
        native_home: home.to_path_buf(),
        session: SessionAction::New {
            session_id: "01993ab0-0000-7000-8000-000000000003".into(),
        },
        launch_id: Id::new("launch").unwrap(),
        binary,
        setting_sources: None,
    }
}

fn pin_source(path: &Path) -> BinarySource {
    BinarySource::Pinned(pin_binary(path).unwrap())
}

#[test]
fn materializer_is_idempotent_for_the_same_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = stub_binary(tmp.path(), "2.1.268 (Claude Code)");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let spec = load_spec();
    let profile = profile();
    let pinned = pin_source(&binary);
    let first = materialize(&request(&spec, &profile, &launch, &home, pinned.clone())).unwrap();
    let second = materialize(&request(&spec, &profile, &launch, &home, pinned)).unwrap();
    assert_eq!(first.argv, second.argv);
    assert_eq!(first.binary, second.binary);
    assert_eq!(first.env_allowlist, second.env_allowlist);
    assert_eq!(
        first.materialized_files[0].content_digest,
        second.materialized_files[0].content_digest
    );
    let settings = fs::read_to_string(&first.materialized_files[0].path).unwrap();
    assert_eq!(
        settings,
        fs::read_to_string(&second.materialized_files[0].path).unwrap()
    );
    assert!(
        first
            .argv
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "default")
    );
    assert!(
        first
            .argv
            .windows(2)
            .any(|w| w[0] == "--setting-sources" && w[1] == "user,project,local")
    );
    assert!(!first.argv.iter().any(|a| a == "--bare"));
}

#[test]
fn recipe_serialization_omits_secret_values_and_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = stub_binary(tmp.path(), "stub-1.0.0");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut spec = load_spec();
    spec.env.insert(
        "ANTHROPIC_API_KEY".into(),
        EnvBinding::Literal(Box::new(LiteralEnv {
            value: "sk-super-secret-value".into(),
            visibility: EnvVisibility::Private,
        })),
    );
    spec.args = vec![];
    let profile = profile();
    let recipe = materialize(&request(
        &spec,
        &profile,
        &launch,
        &home,
        pin_source(&binary),
    ))
    .unwrap();
    let json = serde_json::to_string(&recipe).unwrap();
    assert!(
        !json.contains("sk-super-secret-value"),
        "secret leaked into recipe JSON: {json}"
    );
    let settings = fs::read_to_string(&recipe.materialized_files[0].path).unwrap();
    assert!(!settings.contains("sk-super-secret-value"));
    assert!(!json.to_lowercase().contains("review the diff"));
    assert!(
        recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == "ANTHROPIC_API_KEY" && entry.secret_ref.is_none())
    );
    assert!(
        recipe
            .env_allowlist
            .iter()
            .any(|entry| entry.name == "ANTHROPIC_AUTH_TOKEN"
                && entry.secret_ref.as_deref() == Some("env:REMUDA_TEST_SECRET"))
    );
}

#[test]
fn prohibited_flags_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = stub_binary(tmp.path(), "stub-1.0.0");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let profile = profile();
    for flag in [
        "--bare",
        "--safe-mode",
        "--no-session-persistence",
        "--continue",
    ] {
        let mut spec = load_spec();
        spec.args = vec![flag.into()];
        let error = materialize(&request(
            &spec,
            &profile,
            &launch,
            &home,
            pin_source(&binary),
        ))
        .unwrap_err();
        assert!(
            matches!(
                error,
                DriverError::NativeFeatureDisabled(_) | DriverError::InvalidLaunchSpec(_)
            ),
            "{flag} => {error}"
        );
    }
}

#[test]
fn dont_ask_is_tagged_td_m0_perm_01() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = stub_binary(tmp.path(), "stub-1.0.0");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut spec = load_spec();
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::DontAsk,
        interaction: ClaudeInteractionMode::Host,
    }));
    let recipe = materialize(&request(
        &spec,
        &profile(),
        &launch,
        &home,
        pin_source(&binary),
    ))
    .unwrap();
    assert!(
        recipe
            .technical_debt
            .iter()
            .any(|tag| tag == TECH_DEBT_M0_PERM_01)
    );
    assert!(
        recipe
            .argv
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "dontAsk")
    );
    assert!(
        recipe
            .argv
            .windows(2)
            .any(|w| w[0] == "--permission-prompts" && w[1] == "none")
    );
}

#[test]
fn binary_pin_resolves_stub_and_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stub_binary(tmp.path(), "2.1.268 (Claude Code)");
    let first = pin_binary(&path).unwrap();
    let second = pin_binary(&path).unwrap();
    assert_eq!(first, second);
    assert!(first.abs_path.starts_with('/'));
    assert_eq!(first.version, "2.1.268 (Claude Code)");
}

#[test]
#[ignore = "requires a local claude binary on PATH"]
fn pin_host_claude() {
    let pin = pin_binary("claude").expect("claude must be installed for this ignored test");
    assert!(Path::new(&pin.abs_path).is_file());
    assert!(!pin.version.is_empty());
    assert!(String::from(pin.sha256).starts_with("sha256:"));
}

#[test]
fn recipe_round_trip_json_has_no_env_values() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = stub_binary(tmp.path(), "stub-1.0.0");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let recipe = materialize(&request(
        &load_spec(),
        &profile(),
        &launch,
        &home,
        pin_source(&binary),
    ))
    .unwrap();
    let value = serde_json::to_value(&recipe).unwrap();
    let decoded: LaunchRecipe = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(recipe.argv, decoded.argv);
    assert!(value.get("env").is_none());
}
