//! Assert the hand-maintained OpenAPI 3.1 document covers Hub HTTP routes.
//!
//! Source: `crates/remuda-hub/src/*.rs` `.route(` / `nest_service("/push"` registrations.

use serde_json::Value;
use std::collections::BTreeSet;

fn spec() -> Value {
    serde_json::from_str(include_str!("../openapi/openapi.json")).expect("openapi.json")
}

fn source_route_paths() -> BTreeSet<String> {
    let files = [
        include_str!("../src/http.rs"),
        include_str!("../src/registry.rs"),
        include_str!("../src/devices.rs"),
        include_str!("../src/fleet.rs"),
        include_str!("../src/placement.rs"),
        include_str!("../src/interactions.rs"),
        include_str!("../src/ws.rs"),
        include_str!("../src/lib.rs"),
        include_str!("../src/push_http.rs"),
        include_str!("../src/providers.rs"),
    ];
    let mut paths = BTreeSet::new();
    for src in files {
        let mut rest = src;
        while let Some(idx) = rest.find(".route(") {
            rest = &rest[idx + ".route(".len()..];
            let rest_trim = rest.trim_start();
            if !rest_trim.starts_with('"') {
                continue;
            }
            rest = &rest_trim[1..];
            if let Some(end) = rest.find('"') {
                let path = &rest[..end];
                if path.starts_with('/') {
                    paths.insert(path.to_string());
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
        rest = src;
        while let Some(idx) = rest.find("nest_service(\"") {
            rest = &rest[idx + "nest_service(\"".len()..];
            if let Some(end) = rest.find('"') {
                let path = &rest[..end];
                if path.starts_with('/') {
                    paths.insert(path.to_string());
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
    }
    paths.insert("/push/config".into());
    paths.insert("/push/subscriptions".into());
    paths
}

#[test]
fn openapi_is_31_and_covers_source_routes() {
    let spec = spec();
    assert_eq!(spec["openapi"].as_str(), Some("3.1.0"));
    let paths = spec["paths"].as_object().expect("paths");
    let documented: BTreeSet<String> = paths.keys().cloned().collect();
    let source = source_route_paths();
    for path in &source {
        if path == "/push" {
            continue;
        }
        assert!(
            documented.contains(path),
            "openapi.json missing source route {path}"
        );
    }
    for path in &documented {
        assert!(
            source.contains(path) || path.starts_with("/push/"),
            "openapi.json has extra path {path} not found in source"
        );
    }
}

#[test]
fn overlapping_web_client_operations_exist() {
    let spec = spec();
    let paths = spec["paths"].as_object().expect("paths");
    for (path, method) in [
        ("/v1/hosts", "get"),
        ("/v1/hosts/{id}", "get"),
        ("/v1/instances", "get"),
        ("/v1/instances", "post"),
        ("/v1/instances/{id}", "get"),
        ("/v1/instances/{id}/commands", "post"),
        ("/v1/instances/{id}/journal", "get"),
        ("/v1/worktrees", "get"),
        ("/v1/worktrees", "post"),
        ("/v1/interactions", "get"),
        ("/v1/interactions/{id}/answer", "post"),
        ("/v1/login", "post"),
        ("/healthz", "get"),
        ("/v1/follow", "get"),
    ] {
        assert!(
            paths.get(path).and_then(|p| p.get(method)).is_some(),
            "missing {method} {path}"
        );
    }
}
