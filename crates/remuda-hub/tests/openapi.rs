//! Assert the hand-maintained OpenAPI 3.1 document covers Hub HTTP routes.
//!
//! Source: `crates/remuda-hub/src/*.rs` `.route(` / `nest_service("/push"` registrations.
//!
//! The scan carries the HTTP **method** as well as the path, and the
//! `CommandRecord` schema is diffed against the struct as it actually
//! serializes, so neither a method nor a response field can drift unnoticed.

use serde_json::Value;
use std::collections::BTreeSet;

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::from_str(include_str!("../openapi/openapi.json")).expect("openapi.json")
}

/// Return the byte index just past the opening paren at `open`, after matching
/// the closing `)` while respecting nested parens, strings and char/block/line
/// comments.
fn match_parens(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split an argument list on commas that sit at nesting depth zero.
fn split_top_args(inside: &str) -> Vec<&str> {
    let bytes = inside.as_bytes();
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b',' if depth == 0 => {
                args.push(inside[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = inside[start..].trim();
    if !tail.is_empty() {
        args.push(tail);
    }
    args
}

/// Every `(method, path)` the Hub actually registers, plus every path a method
/// router is mounted on (so a route with a changed-but-unscanned method is
/// still seen).
fn source_routes() -> BTreeSet<(String, String)> {
    // Discover feature modules so registering a route never needs a second
    // registration in this test. Skip inline test fixtures below #[cfg(test)].
    let files = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| std::fs::read_to_string(path).unwrap());
    let mut routes = BTreeSet::new();
    for src in files {
        let src = src.split("#[cfg(test)]").next().unwrap();
        let mut cursor = 0usize;
        while let Some(rel) = src[cursor..].find(".route(") {
            let open = cursor + rel + ".route".len();
            let Some(end) = match_parens(src, open) else {
                break;
            };
            let inside = &src[open + 1..end - 1];
            let args = split_top_args(inside);
            if let Some(path) = args
                .first()
                .and_then(|a| a.strip_prefix('"'))
                .and_then(|a| a.strip_suffix('"'))
                .filter(|p| p.starts_with('/'))
            {
                let services = args[1..].join(",");
                let mut matched = Vec::new();
                for method in METHODS {
                    let needle = format!("{method}(");
                    for (idx, _) in services.match_indices(&needle) {
                        // Require a non-identifier char before the method name
                        // (so `forget(` does not match `get(`) — the name always
                        // follows `.`, `:`, whitespace or the argument start.
                        let boundary = idx == 0 || !ident_char(services.as_bytes()[idx - 1]);
                        if boundary {
                            matched.push(method.to_string());
                        }
                    }
                }
                matched.sort();
                matched.dedup();
                if matched.is_empty() {
                    // Mounted/opaque service (e.g. nest); record path only.
                    routes.insert((String::new(), path.to_string()));
                } else {
                    for method in matched {
                        routes.insert((method, path.to_string()));
                    }
                }
            }
            cursor = end;
        }
    }
    routes
}

fn ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[test]
fn openapi_is_31_and_covers_source_routes_and_methods() {
    let spec = spec();
    assert_eq!(spec["openapi"].as_str(), Some("3.1.0"));
    let paths = spec["paths"].as_object().expect("paths");

    // The push carrier is nest-mounted, not .route-registered, so pin its
    // documented subpaths explicitly.
    for push_path in ["/push/config", "/push/subscriptions"] {
        assert!(
            paths.contains_key(push_path),
            "openapi.json missing {push_path}"
        );
    }

    // Documented (method, path) operations.
    let mut documented = BTreeSet::new();
    for (path, item) in paths {
        for method in METHODS {
            if item.get(method).is_some() {
                documented.insert((method.to_string(), path.clone()));
            }
        }
    }

    let source = source_routes();
    for (method, path) in &source {
        if path == "/push" || path.starts_with("/push/") || method.is_empty() {
            continue;
        }
        assert!(
            documented.contains(&(method.clone(), path.clone())),
            "openapi.json missing source route {method} {path}"
        );
    }
    for (method, path) in &documented {
        if path == "/push" || path.starts_with("/push/") {
            // Push carrier operations are mounted, not registered with .route.
            continue;
        }
        let in_source = source
            .iter()
            .any(|(m, p)| p == path && (m == method || m.is_empty()));
        assert!(
            in_source,
            "openapi.json documents {method} {path} but no such source route exists"
        );
    }
}

#[test]
fn command_record_schema_matches_the_serialized_struct() {
    // The shape the listing/POST actually emits (every Option populated so
    // `skip_serializing_if` fields appear).
    let sample = remuda_hub::store_test_support::sample_command_record_json();
    let serialized: BTreeSet<String> = sample
        .as_object()
        .expect("record is an object")
        .keys()
        .cloned()
        .collect();

    let schema = &spec()["components"]["schemas"]["CommandRecord"];
    let documented: BTreeSet<String> = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect();

    for field in &serialized {
        assert!(
            documented.contains(field),
            "CommandRecord emits `{field}` but openapi.json does not document it"
        );
    }
    for field in &documented {
        assert!(
            serialized.contains(field),
            "openapi.json documents CommandRecord.{field} but the struct does not serialize it"
        );
    }

    // §2.5 / §12.2 vocabulary is pinned on the wire: exactly three progress
    // states and three resolution states — the bug this guards emitted a
    // fourth (`failed`) and the protocol crate could not parse it.
    fn enum_values(schema: &Value, pointer: &str) -> BTreeSet<String> {
        schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
    assert_eq!(
        enum_values(schema, "/properties/state/enum"),
        BTreeSet::from(["accepted".into(), "queued".into(), "settled".into()]),
        "Command.state must stay the §2.5 three-state vocabulary"
    );
    assert_eq!(
        enum_values(schema, "/properties/resolution/enum"),
        BTreeSet::from(["clear".into(), "reconciling".into(), "unknown".into()]),
        "Command.resolution must stay the §12.2 vocabulary"
    );
    // The settlement outcome enum must equal the *entire* protocol
    // SettlementOutcome set, not merely contain the sample's value: the journal
    // projection persists any outcome the protocol enum parses (including
    // `expired`), so a narrower spec would document a vocabulary the Hub emits.
    assert_eq!(
        enum_values(schema, "/properties/settlement/properties/outcome/enum"),
        remuda_hub::store_test_support::settlement_outcome_wire_values(),
        "settlement.outcome enum must match the protocol SettlementOutcome set"
    );
    // The sample is a rejected settlement; both levels use documented values.
    assert!(
        enum_values(schema, "/properties/settlement/properties/outcome/enum")
            .contains(sample["settlement"]["outcome"].as_str().unwrap())
    );
    assert!(
        enum_values(schema, "/properties/state/enum").contains(sample["state"].as_str().unwrap())
    );

    // Internal ledger columns must never leak to the wire.
    for private in ["settlementOutcome", "settlementReason", "reason"] {
        assert!(
            !documented.contains(private),
            "internal column `{private}` must not be a documented wire field"
        );
    }
}

#[test]
fn overlapping_web_client_operations_exist() {
    let spec = spec();
    let paths = spec["paths"].as_object().expect("paths");
    for (path, method) in [
        ("/v1/hosts", "get"),
        ("/v1/hosts/ssh", "post"),
        ("/v1/hosts/{id}", "delete"),
        ("/v1/hosts/{id}", "get"),
        ("/v1/instances", "get"),
        ("/v1/instances", "post"),
        ("/v1/instances/{id}", "get"),
        ("/v1/instances/{id}/commands", "get"),
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
