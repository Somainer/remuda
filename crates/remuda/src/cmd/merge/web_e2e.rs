//! Authentication changes need the real Hub/browser contract at the merge gate.

pub(super) fn changed(paths: &[u8]) -> bool {
    paths.split(|byte| *byte == 0).any(|path| {
        matches!(
            path,
            b"web/src/lib/api.ts"
                | b"web/src/lib/session.ts"
                | b"web/src/lib/store.ts"
                | b"web/src/lib/accessCode.ts"
                | b"web/src/features/session/tty/client.ts"
                | b"web/playwright.hub.config.ts"
                | b"crates/remuda-hub/src/auth.rs"
                | b"crates/remuda-hub/src/devices.rs"
                | b"crates/remuda-hub/src/http.rs"
                | b"crates/remuda-hub/src/lib.rs"
                | b"crates/remuda-hub/src/rate_limit.rs"
                | b"crates/remuda-hub/src/store.rs"
                | b"crates/remuda-hub/src/ws.rs"
        ) || path.starts_with(b"web/tests/e2e/hub-")
            || path == b"web/tests/e2e/pairing.spec.ts"
    })
}
