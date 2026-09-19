//! D-045 end-to-end through a real driver spawn: a granted launch starts the
//! child with the capability handshake env and the per-instance MCP config on
//! argv; an ungranted launch starts it with neither.
//!
//! The child is a `/bin/sh` stub that records its argv and environment (the
//! same trick as `child_env_isolation.rs`), so this proves the spawn sites
//! applied the recipe — not just that the materializer produced one.

use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Delegation, Driver, FileRole, ProviderHealth, ProviderKind,
    ProviderProfile,
};
use remuda_protocol::Digest;
use remuda_testing::install_executable;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap()
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn load_spec() -> remuda_protocol::InstanceSpec {
    serde_json::from_str(include_str!(
        "../../remuda-driver/tests/fixtures/instance-spec.json"
    ))
    .unwrap()
}

/// A stub `claude` that records argv (one per line) then the environment.
fn recording_binary(dir: &Path, argv_out: &Path, env_out: &Path) -> PathBuf {
    let script = format!(
        "#!/bin/sh\n\
         : > {argv}.pending\n\
         for a in \"$@\"; do printf '%s\\n' \"$a\" >> {argv}.pending; done\n\
         mv {argv}.pending {argv}\n\
         env > {env}.pending\n\
         mv {env}.pending {env}\n\
         exit 0\n",
        argv = argv_out.display(),
        env = env_out.display()
    );
    install_executable(dir, "claude", script)
}

struct Captured {
    argv: String,
    env: BTreeMap<String, String>,
}

async fn launch(granted: bool) -> (tempfile::TempDir, Captured, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let argv_out = tmp.path().join("argv.txt");
    let env_out = tmp.path().join("env.txt");
    let binary = recording_binary(tmp.path(), &argv_out, &env_out);
    let pin = BinaryPin {
        abs_path: binary
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        version: "stub".into(),
        sha256: dummy_digest(),
    };

    let mut spec = load_spec();
    spec.cwd = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if granted {
        spec.capabilities = vec!["computer-use".to_owned()];
    }

    let mut options =
        ClaudePrintOptions::new(profile(), launch.clone(), home, BinarySource::Pinned(pin));
    options.handshake_timeout = Duration::from_secs(10);
    options.origin = remuda_protocol::InputOrigin::Human;
    let driver: Box<dyn Driver> = Box::new(ClaudePrintDriver::new(options));
    // The stub exits immediately, so the handshake errors; it has already
    // written argv and env, which is the whole assertion.
    let _ = driver.start(spec).await;

    for _ in 0..80 {
        if argv_out.is_file() && env_out.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let argv = std::fs::read_to_string(&argv_out).expect("child recorded argv");
    let env_raw = std::fs::read_to_string(&env_out).expect("child recorded env");
    let env = env_raw
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect();
    let mcp_config = launch.join("mcp-cua.json");
    let recipe_files = if mcp_config.is_file() {
        vec![mcp_config.to_string_lossy().into_owned()]
    } else {
        Vec::new()
    };
    (tmp, Captured { argv, env }, recipe_files)
}

#[tokio::test]
async fn granted_launch_child_sees_handshake_env_and_mcp_config() {
    let (_tmp, captured, files) = launch(true).await;
    assert_eq!(
        captured
            .env
            .get("REMUDA_CAPABILITY_COMPUTER_USE")
            .map(String::as_str),
        Some("1"),
        "child env must carry the granted handshake: {:#?}",
        captured.env.keys().collect::<Vec<_>>()
    );
    let mcp_config = files
        .into_iter()
        .next()
        .map(PathBuf::from)
        .expect("mcp-cua.json was materialized");
    assert!(
        captured
            .argv
            .contains(&mcp_config.to_string_lossy().into_owned()),
        "child argv must name the per-instance MCP config: {}",
        captured.argv
    );
    // The config itself names the embedded launcher and carries the handshake.
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mcp_config).unwrap()).unwrap();
    assert!(
        config["mcpServers"]["codex-computer-use"]["args"][0]
            .as_str()
            .unwrap()
            .ends_with("launch-cua-repl.sh")
    );
    // The materialized launcher on disk is the embedded one and executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let launcher = mcp_config
            .parent()
            .unwrap()
            .join("cua/scripts/launch-cua-repl.sh");
        assert_eq!(
            std::fs::metadata(&launcher).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(
            std::fs::read_to_string(&launcher)
                .unwrap()
                .contains("REMUDA_CAPABILITY_COMPUTER_USE")
        );
    }
    // The role tag is right (compile-time touch of the audit enum).
    let _ = FileRole::CapabilityMcpConfig;
}

#[tokio::test]
async fn ungranted_launch_child_sees_neither() {
    let (_tmp, captured, files) = launch(false).await;
    assert!(
        !captured.env.contains_key("REMUDA_CAPABILITY_COMPUTER_USE"),
        "no grant, no handshake"
    );
    assert!(
        !captured
            .argv
            .lines()
            .any(|line| line.ends_with("mcp-cua.json")),
        "no grant, no mcp config on argv: {}",
        captured.argv
    );
    assert!(files.is_empty());
}
