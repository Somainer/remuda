//! Source: `protocol.md` §4.4 / D-012. Fixture is synthetic.

use remuda_protocol::*;
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider-profile.json");
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn provider_profile_round_trip_omits_the_token() {
    let value = fixture();
    let profile: ProviderProfile = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(profile.kind, ProviderProfileKind::Gateway);
    assert!(profile.default_gateway);
    assert_eq!(profile.secret.last4.as_deref(), Some("t0k1"));
    assert_eq!(serde_json::to_value(&profile).unwrap(), value);
    let blob = serde_json::to_string(&profile).unwrap();
    assert!(!blob.to_lowercase().contains("sk-"));
    assert!(!blob.contains("authToken"));
}

#[test]
fn provider_overlay_spec_has_no_secret_field() {
    let spec = ProviderOverlaySpec {
        profile_id: "pvp_01993ab0-0000-7000-8000-000000000010".parse().unwrap(),
        kind: ProviderProfileKind::Direct,
        base_url: String::new(),
        model: "claude-sonnet".into(),
        headers: Default::default(),
        scope: "universal".into(),
    };
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["kind"], json!("direct"));
    assert!(value.get("secret").is_none());
    assert!(value.get("authToken").is_none());
}
