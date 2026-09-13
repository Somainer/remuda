//! Authentication and session-composer changes need the real Hub/browser contract at the merge gate.

pub(super) fn changed(paths: &[u8]) -> bool {
    paths.split(|byte| *byte == 0).any(path_enables)
}

fn path_enables(path: &[u8]) -> bool {
    matches!(
        path,
        b"web/src/lib/api.ts"
            | b"web/src/lib/session.ts"
            | b"web/src/lib/store.ts"
            | b"web/src/lib/accessCode.ts"
            | b"web/playwright.hub.config.ts"
            | b"crates/remuda-hub/src/auth.rs"
            | b"crates/remuda-hub/src/devices.rs"
            | b"crates/remuda-hub/src/http.rs"
            | b"crates/remuda-hub/src/lib.rs"
            | b"crates/remuda-hub/src/rate_limit.rs"
            | b"crates/remuda-hub/src/store.rs"
            | b"crates/remuda-hub/src/ws.rs"
    ) || path.starts_with(b"web/src/features/session/")
        || path.starts_with(b"web/tests/e2e/hub-")
        || path == b"web/tests/e2e/pairing.spec.ts"
}

#[cfg(test)]
mod tests {
    use super::changed;

    #[test]
    fn session_api_and_hub_auth_paths_auto_enable_live_e2e() {
        for path in [
            "web/src/lib/api.ts",
            "web/src/lib/session.ts",
            "web/src/features/session/Composer.tsx",
            "web/src/features/session/effort.ts",
            "web/src/features/session/tty/client.ts",
            "crates/remuda-hub/src/auth.rs",
            "crates/remuda-hub/src/http.rs",
            "web/tests/e2e/hub-live.spec.ts",
            "web/tests/e2e/pairing.spec.ts",
        ] {
            let mut bytes = path.as_bytes().to_vec();
            bytes.push(0);
            assert!(changed(&bytes), "{path}");
        }
        assert!(!changed(b"web/src/pages/HostsPage.tsx\0"));
        assert!(!changed(b"web/a new file.txt\0"));
        assert!(!changed(b"crates/remuda/src/cmd/merge.rs\0"));
        assert!(!changed(b""));
    }
}
