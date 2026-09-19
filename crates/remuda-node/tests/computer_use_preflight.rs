//! D-045 Node boundary: a `computer-use` grant is refused on a host that is not
//! macOS or whose inventory lacks an installed `computer-use` row. The
//! capability row is owned by the hostcap batch; until it exists every grant
//! fails closed with the "not reported" message rather than materializing.
//!
//! These exercise the pure gate (`evaluate`) with injected host facts — the
//! workspace forbids `unsafe`, so in-process `set_var` is unavailable and the
//! live host facts (`consts::OS`, the probe) stay out of the tested core.

use remuda_node::{CliAuth, CliEntry};
use remuda_protocol::AgentKind;
use std::path::PathBuf;

fn row(installed: bool, path: Option<&str>) -> CliEntry {
    CliEntry {
        kind: "computer-use".to_owned(),
        version: None,
        path: path.map(PathBuf::from),
        auth: CliAuth::Unknown,
        installed,
        native_gateway: None,
        sha256: None,
    }
}

#[test]
fn missing_row_refuses_with_the_not_reported_message() {
    let error = remuda_node::computer_use_evaluate(&AgentKind::Claude, "macos", None).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("computer-use"), "{message}");
    assert!(message.contains("not reported"), "{message}");
}

#[test]
fn non_macos_refuses_and_names_the_os_before_looking_at_the_row() {
    let error = remuda_node::computer_use_evaluate(&AgentKind::Claude, "linux", None).unwrap_err();
    assert!(error.to_string().contains("linux"), "{error}");
    assert!(error.to_string().contains("macOS"), "{error}");

    // Even an installed row does not make a non-macOS host eligible.
    let installed = row(true, Some("/opt/cua"));
    let error = remuda_node::computer_use_evaluate(&AgentKind::Claude, "windows", Some(&installed))
        .unwrap_err();
    assert!(error.to_string().contains("windows"), "{error}");
}

#[test]
fn installed_row_on_macos_passes_for_claude_and_codex() {
    let installed = row(
        true,
        Some(
            "/Users/u/.codex/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient",
        ),
    );
    remuda_node::computer_use_evaluate(&AgentKind::Claude, "macos", Some(&installed)).unwrap();
    remuda_node::computer_use_evaluate(&AgentKind::Codex, "macos", Some(&installed)).unwrap();
}

#[test]
fn not_installed_row_refuses_and_names_the_probed_path() {
    let absent = row(
        false,
        Some("/Users/u/.codex/computer-use/SkyComputerUseClient"),
    );
    let error =
        remuda_node::computer_use_evaluate(&AgentKind::Claude, "macos", Some(&absent)).unwrap_err();
    assert!(error.to_string().contains("not installed"), "{error}");
    assert!(
        error.to_string().contains("SkyComputerUseClient"),
        "{error}"
    );
}

#[test]
fn not_installed_row_without_path_names_the_default_probe_location() {
    // Round-5 item 5: an installed=false row with no path must not print the
    // unhelpful `<no path reported>` placeholder; it names the default client
    // path the hostcap probe resolves (when HOME is set, as it is in CI).
    let absent = row(false, None);
    let error =
        remuda_node::computer_use_evaluate(&AgentKind::Claude, "macos", Some(&absent)).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("not installed"), "{message}");
    assert!(
        !message.contains("<no path reported>"),
        "must fall back to the default probe path: {message}"
    );
    if let Some(path) = remuda_node::computer_use_default_client_path() {
        assert!(
            message.contains(&path.to_string_lossy().to_string()),
            "message must name the resolved default path {path:?}: {message}"
        );
        assert!(
            path.to_string_lossy().contains("computer-use"),
            "default path points at the CUA app: {path:?}"
        );
    }
}

#[test]
fn default_client_path_points_at_the_cua_app_under_codex_home() {
    let path = remuda_node::computer_use_default_client_path();
    // CI always has HOME; the path must be the canonical app bundle location.
    if let Some(path) = path {
        let text = path.to_string_lossy();
        assert!(
            text.ends_with("computer-use/Codex Computer Use.app"),
            "{text}"
        );
    }
}

#[test]
fn unsupported_kinds_refuse_even_with_an_installed_macos_row() {
    let installed = row(true, Some("/opt/cua"));
    for kind in [AgentKind::Grok, AgentKind::Agy, AgentKind::Terminal] {
        let error =
            remuda_node::computer_use_evaluate(&kind, "macos", Some(&installed)).unwrap_err();
        assert!(
            error.to_string().contains("not supported"),
            "{kind:?}: {error}"
        );
    }
}
