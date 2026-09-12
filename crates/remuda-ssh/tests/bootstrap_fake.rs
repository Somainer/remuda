//! Bootstrap skip/upload against `tests/fixtures/fake-ssh.sh`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use remuda_ssh::{BootstrapResult, SshClient, SshOptions, bootstrap, sha256_file};

fn fake_ssh() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("fake-ssh.sh");
    ensure_exec(&path);
    path
}

fn ensure_exec(path: &Path) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[tokio::test]
async fn uploads_then_skips_matching_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("remuda");
    std::fs::write(
        &local,
        "#!/bin/sh\necho remuda 0.1.0-test\necho commit=fixture\n",
    )
    .unwrap();
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();

    let dest = tmp.path().join("remote").join("bin").join("remuda");
    let dest_s = dest.to_string_lossy().into_owned();

    let mut options = SshOptions::keepalive();
    options.ssh_binary = fake_ssh();
    options.connect_timeout_secs = Some(2);
    options.runtime_dir = tmp.path().join("cm");
    let client = SshClient::new("devbox-sg", options);

    let first = bootstrap(&client, &local, &dest_s).await.unwrap();
    match first {
        BootstrapResult::Uploaded {
            digest,
            remote_path,
            version,
        } => {
            assert_eq!(digest, sha256_file(&local).unwrap());
            assert_eq!(remote_path, dest_s);
            assert!(version.contains("remuda 0.1.0-test"), "{version}");
        }
        BootstrapResult::Skipped { .. } => panic!("expected first upload"),
    }

    let second = bootstrap(&client, &local, &dest_s).await.unwrap();
    assert!(matches!(second, BootstrapResult::Skipped { .. }));
}
