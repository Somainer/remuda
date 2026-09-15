//! `ssh -G` and `~/.ssh/config` Host listing. Fixtures: `tests/fixtures/SOURCES.md`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use remuda_ssh::{SshTarget, list_config_hosts};

fn ensure_exec(path: &Path) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn parses_devbox_sg_g_output() {
    let target = SshTarget::from_g_output("devbox-sg", &fixture("ssh-G-devbox-sg.txt")).unwrap();
    assert_eq!(target.alias, "devbox-sg");
    assert_eq!(target.hostname, "10.199.x.x");
    assert_eq!(target.user, "devuser");
    assert_eq!(target.port, 34387);
    assert_eq!(target.proxy_jump, None);
    assert!(
        target
            .identity_files
            .iter()
            .any(|p| p.ends_with(".ssh/id_ed25519"))
    );
}

#[test]
fn parses_proxyjump_and_strips_user_comment() {
    let target =
        SshTarget::from_g_output("forge-doloris", &fixture("ssh-G-forge-doloris.txt")).unwrap();
    assert_eq!(target.user, "root");
    assert_eq!(target.hostname, "10.102.x.x");
    assert_eq!(target.port, 22);
    assert_eq!(
        target.proxy_jump.as_deref(),
        Some("jump-proxy-us.tiktok-row.org")
    );
}

#[test]
fn lists_hosts_excluding_wildcards_and_follows_include() {
    let hosts = list_config_hosts(&fixture_path("ssh-config.txt")).unwrap();
    assert_eq!(
        hosts,
        vec![
            "extra-box".to_string(),
            "devbox".to_string(),
            "devbox-sg".to_string(),
            "forge-doloris".to_string(),
            "devbox-sg-host".to_string(),
        ]
    );
    assert!(!hosts.iter().any(|h| h.contains('*') || h.contains('?')));
}

#[test]
fn resolve_with_fake_ssh_reads_g_fixture() {
    let script = fixture_path("fake-ssh.sh");
    ensure_exec(&script);
    let target = SshTarget::resolve_with(&script, "devbox-sg").unwrap();
    assert_eq!(target.hostname, "10.199.x.x");
    assert_eq!(target.port, 34387);
}
