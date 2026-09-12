//! `security-review-2.md` S1/S2: an agent child must not inherit the Node's
//! environment.
//!
//! The proof is end-to-end rather than a check on the map the driver builds.
//! A "node" process is spawned holding real secrets in its environment; it
//! launches an agent through `ClaudePrintDriver`, and the agent binary dumps
//! the environment it actually received. A regression that drops `env_clear`,
//! or adds a spawn site that forgets it, fails here.
//!
//! The two layers are the same test binary: [`agent_child_is_isolated`] is the
//! node and runs only when re-executed by [`node_environment_is_not_inherited`]
//! with the secrets set. The crate forbids `unsafe`, so the environment is
//! established with `Command::env` on that re-exec rather than `set_var`.

use remuda_driver::child_env;
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinaryPin, BinarySource, Delegation, Driver, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{Digest, InstanceSpec};
use remuda_testing::install_executable;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Set on the re-exec so the inner test knows it is the node.
const NODE_MARKER: &str = "REMUDA_S1_ISOLATION_NODE";

/// Secrets a real Node holds. None may reach the agent child.
///
/// Every name here is inert: nothing in the OS or the toolchain acts on it, so
/// planting it in a live process is safe on both Linux and macOS.
const NODE_SECRETS: &[(&str, &str)] = &[
    ("REMUDA_BOOTSTRAP_TOKEN", "rmd-bootstrap-leak-canary"),
    ("REMUDA_HOST_TOKEN", "rmd-host-leak-canary"),
    ("ANTHROPIC_API_KEY", "sk-ant-leak-canary"),
    ("ANTHROPIC_AUTH_TOKEN", "sk-auth-leak-canary"),
    ("AWS_SECRET_ACCESS_KEY", "aws-leak-canary"),
    ("GITHUB_TOKEN", "ghp-leak-canary"),
];

/// Hostile names that are safe to set on a *live* process.
///
/// Proxy and TLS variables are read by HTTP clients, not by the loader, so a
/// `/bin/sh` stub ignores them entirely.
const HOSTILE_INERT: &[(&str, &str)] = &[
    ("HTTPS_PROXY", "http://mitm.example:8080"),
    ("https_proxy", "http://mitm.example:8080"),
    ("SSL_CERT_FILE", "/tmp/mitm.pem"),
    ("NODE_EXTRA_CA_CERTS", "/tmp/mitm.pem"),
];

/// Loader names, which must **never** be set on a live process in this test.
///
/// macOS dyld terminates any process started with `DYLD_INSERT_LIBRARIES`
/// naming a dylib it cannot load, so planting these would kill the spawned
/// child before it runs and the leak assertion would misread the corpse as
/// inheritance. They are proven through [`child_env`]'s pure filter instead —
/// which is the same code path the spawn sites use, so the coverage is real.
/// (Asserting their absence after a spawn would also be vacuous on macOS,
/// where SIP strips `DYLD_*` when exec'ing a protected binary like `/bin/sh`.)
const HOSTILE_LOADER: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "BASH_ENV",
    "GIT_SSH_COMMAND",
];

fn dummy_digest() -> Digest {
    Digest::try_from(format!("sha256:{:0>64}", "a")).unwrap()
}

/// A stub `claude` that writes its environment to `dump` and exits.
fn env_dump_binary(dir: &Path, dump: &Path) -> PathBuf {
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then echo '2.1.268 (Claude Code)'; exit 0; fi\n\
         env > '{}'\n\
         exit 0\n",
        dump.display()
    );
    install_executable(dir, "claude", script)
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

fn load_spec(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("fixtures/instance-spec.json")).unwrap();
    spec.cwd = cwd.canonicalize().unwrap().to_string_lossy().into_owned();
    spec
}

fn read_dump(path: &Path) -> BTreeMap<String, String> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read env dump {}: {err}", path.display()));
    raw.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Launch one agent child and return the environment it actually received.
async fn spawn_and_capture_env(extra_env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let tmp = tempfile::tempdir().unwrap();
    let dump = tmp.path().join("env-dump.txt");
    let launch = tmp.path().join("launch");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let binary = env_dump_binary(tmp.path(), &dump);

    let pin = BinaryPin {
        abs_path: binary
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        version: "2.1.268 (Claude Code)".into(),
        sha256: dummy_digest(),
    };
    let mut options = ClaudePrintOptions::new(profile(), launch, home, BinarySource::Pinned(pin));
    options.extra_env = extra_env;
    options.handshake_timeout = Duration::from_millis(1500);
    let driver = ClaudePrintDriver::new(options);

    // The stub exits at once so the handshake fails, but it has already
    // written its environment — which is the whole assertion.
    let _ = driver.start(load_spec(tmp.path())).await;

    for _ in 0..40 {
        if dump.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        dump.is_file(),
        "the stub never ran; nothing to assert about its environment"
    );
    read_dump(&dump)
}

/// The node half. Ignored by default; run by the outer test with secrets set.
#[tokio::test]
#[ignore = "re-executed by node_environment_is_not_inherited with a hostile env"]
async fn agent_child_is_isolated() {
    assert!(
        std::env::var(NODE_MARKER).is_ok(),
        "this test must be re-executed by node_environment_is_not_inherited"
    );
    // Sanity: this process really does hold the variables we expect dropped.
    // Without this the test could pass simply because they were never set.
    for (name, value) in NODE_SECRETS.iter().chain(HOSTILE_INERT) {
        assert_eq!(
            std::env::var(name).ok().as_deref(),
            Some(*value),
            "the node process is missing {name}; the test would prove nothing"
        );
    }

    let env = spawn_and_capture_env(BTreeMap::new()).await;

    // S1: no Node secret reaches the child, by name or by value. These are
    // names this test set itself, so the assertion is about our own code and
    // not about whatever the platform injects.
    for (name, value) in NODE_SECRETS {
        assert!(
            !env.contains_key(*name),
            "child inherited {name}; full env: {env:#?}"
        );
        assert!(
            !env.values().any(|actual| actual.contains(value)),
            "the value of {name} reached the child under another name: {env:#?}"
        );
    }
    assert!(
        !env.keys().any(|name| name.starts_with("REMUDA_")),
        "a REMUDA_* variable reached the child: {env:#?}"
    );

    // S2: no proxy or TLS override reaches the child. Loader variables are
    // covered by `loader_names_are_denied_by_the_filter` — see HOSTILE_LOADER
    // for why they must not be planted in a live process.
    for (name, _) in HOSTILE_INERT {
        assert!(
            !env.contains_key(*name),
            "child inherited {name}; full env: {env:#?}"
        );
    }

    // An empty environment would pass the checks above while breaking every
    // real launch, so assert the child is still usable.
    for required in ["PATH", "HOME"] {
        assert!(
            env.contains_key(required),
            "child is missing {required}; full env: {env:#?}"
        );
    }
    assert!(
        env.contains_key("CLAUDE_CONFIG_DIR"),
        "the materialized native home did not reach the child: {env:#?}"
    );
}

/// Spawn the node half with a Node-like environment and require it to pass.
#[test]
fn node_environment_is_not_inherited() {
    let exe = std::env::current_exe().expect("test binary");
    let mut command = std::process::Command::new(exe);
    command
        .args([
            "--exact",
            "agent_child_is_isolated",
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(NODE_MARKER, "1");
    // Only inert names: see HOSTILE_LOADER for why no loader variable may be
    // set on a process that is about to exec.
    for (name, value) in NODE_SECRETS.iter().chain(HOSTILE_INERT) {
        command.env(name, value);
    }
    let output = command.output().expect("re-exec the test binary");
    assert!(
        output.status.success(),
        "the agent child inherited something it should not have\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // Guard against the inner test silently not running (a rename, a harness
    // change): a pass with zero tests executed would otherwise look green.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "the inner test did not run: {stdout}"
    );
}

/// The loader family, proven against the filter every spawn site calls.
///
/// This is a pure-function test by necessity, not convenience: see
/// [`HOSTILE_LOADER`]. `is_denied` is the same gate `base_env` and each
/// driver's `extra_env` loop use, so a regression here is a real regression.
#[test]
fn loader_names_are_denied_by_the_filter() {
    for name in HOSTILE_LOADER {
        assert!(child_env::is_denied(name), "{name} is not denied");
        // And a parent environment carrying it yields nothing for the child.
        let inherited = child_env::inherit_from([
            ((*name).to_owned(), "/tmp/evil".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]);
        assert_eq!(
            inherited.keys().collect::<Vec<_>>(),
            vec!["PATH"],
            "{name} survived the inherit filter"
        );
    }
}

#[tokio::test]
async fn extra_env_cannot_reintroduce_a_denied_name() {
    // A caller-supplied overlay is subject to the same denylist. `LD_PRELOAD`
    // is safe to use here even on macOS: it goes through `extra_env`, which
    // the driver filters before building the child, so no process is ever
    // exec'd with it set. (Contrast HOSTILE_LOADER, which would be inherited.)
    let mut extra = BTreeMap::new();
    extra.insert("LD_PRELOAD".to_owned(), "/tmp/injected.so".to_owned());
    extra.insert("HTTPS_PROXY".to_owned(), "http://injected:8080".to_owned());
    extra.insert("FAKE_CLAUDE_SCRIPT".to_owned(), "/tmp/script".to_owned());
    let env = spawn_and_capture_env(extra).await;

    assert!(!env.contains_key("LD_PRELOAD"), "{env:#?}");
    assert!(!env.contains_key("HTTPS_PROXY"), "{env:#?}");
    // A benign overlay still works — the denylist is not a blanket ban.
    assert_eq!(
        env.get("FAKE_CLAUDE_SCRIPT").map(String::as_str),
        Some("/tmp/script"),
        "a permitted extra_env entry was dropped: {env:#?}"
    );
}

#[test]
fn denylist_covers_every_name_this_test_plants() {
    for (name, _) in NODE_SECRETS
        .iter()
        .filter(|(n, _)| n.starts_with("REMUDA_"))
    {
        assert!(child_env::is_denied(name), "{name} is not denied");
    }
    for (name, _) in HOSTILE_INERT {
        assert!(child_env::is_denied(name), "{name} is not denied");
    }
    for name in HOSTILE_LOADER {
        assert!(child_env::is_denied(name), "{name} is not denied");
    }
}
