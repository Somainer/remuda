//! Source: `protocol.md` §4.4 / D-012. Fixture is synthetic.

use remuda_protocol::*;
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider-profile.json");
    from_json_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn host_id() -> HostId {
    "hst_01993ab0-0000-7000-8000-000000000007".parse().unwrap()
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
        delivery: ProviderDelivery::direct(),
    };
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["kind"], json!("direct"));
    assert!(value.get("secret").is_none());
    assert!(value.get("authToken").is_none());
    // The D2 default is omitted, so a direct profile's bytes are unchanged.
    assert!(value.get("delivery").is_none());
}

// ── D-047 delivery (2026-09-19) ────────────────────────────────────────────

/// The whole point of the additive rule: the fixture above predates D-047 and
/// carries no `delivery` key. It must keep parsing, and it must mean `direct`.
#[test]
fn profile_without_delivery_parses_as_direct_auto() {
    let value = fixture();
    assert!(value.get("delivery").is_none());
    let profile: ProviderProfile = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(profile.delivery, ProviderDelivery::direct());
    assert_eq!(profile.delivery.mode, ProviderDeliveryMode::Direct);
    assert_eq!(profile.delivery.via_host_id, None);
    assert_eq!(profile.delivery.route, ApiRouteMode::Auto);
    assert!(!profile.delivery.is_via());
    // Round-trips byte-identically: reading and re-writing adds no key.
    assert_eq!(serde_json::to_value(&profile).unwrap(), value);
}

#[test]
fn provider_delivery_via_round_trips_with_its_route() {
    let mut value = fixture();
    value["delivery"] = json!({
        "mode": "via",
        "viaHostId": host_id().as_id().as_str(),
        "route": "hub-relay",
    });
    let profile: ProviderProfile = serde_json::from_value(value.clone()).unwrap();
    assert!(profile.delivery.is_via());
    assert_eq!(profile.delivery.via_host_id, Some(host_id()));
    assert_eq!(profile.delivery.route, ApiRouteMode::HubRelay);
    assert_eq!(serde_json::to_value(&profile).unwrap(), value);
}

/// Each route value is its own wire word, and `auto` is the omitted default.
#[test]
fn provider_delivery_route_values_are_exact_and_default_to_auto() {
    for (wire, mode) in [
        ("auto", ApiRouteMode::Auto),
        ("hub-relay", ApiRouteMode::HubRelay),
        ("direct-net", ApiRouteMode::DirectNet),
    ] {
        let delivery: ProviderDelivery = serde_json::from_value(json!({
            "mode": "via",
            "viaHostId": host_id().as_id().as_str(),
            "route": wire,
        }))
        .unwrap();
        assert_eq!(delivery.route, mode);
        assert_eq!(delivery.route.as_str(), wire);
        assert_eq!(
            serde_json::to_value(&delivery).unwrap()["route"],
            json!(wire)
        );
    }
    // `route` omitted entirely: auto, per Amendment A1.
    let delivery: ProviderDelivery = serde_json::from_value(json!({
        "mode": "via",
        "viaHostId": host_id().as_id().as_str(),
    }))
    .unwrap();
    assert_eq!(delivery.route, ApiRouteMode::Auto);
}

/// A typo'd route must not quietly become `auto`: that would move a session's
/// egress by accident, which is exactly what refuse-never-reroute forbids.
#[test]
fn unknown_route_and_mode_are_parse_errors_not_defaults() {
    let unknown_route = serde_json::from_value::<ProviderDelivery>(json!({
        "mode": "via",
        "viaHostId": host_id().as_id().as_str(),
        "route": "hub_relay",
    }));
    assert!(unknown_route.is_err(), "hub_relay must not parse");

    let unknown_mode = serde_json::from_value::<ProviderDelivery>(json!({
        "mode": "tunnel",
        "viaHostId": host_id().as_id().as_str(),
    }));
    assert!(unknown_mode.is_err(), "tunnel must not parse");

    // And the same rule reaches a whole profile, not just the inner type.
    let mut value = fixture();
    value["delivery"] = json!({ "mode": "via", "route": "nope" });
    assert!(serde_json::from_value::<ProviderProfile>(value).is_err());
}

/// `via` without a host cannot be honoured. The type still deserializes (so a
/// stored row is readable and can be repaired) but reports itself invalid,
/// which is what the Hub refuses on.
#[test]
fn via_without_a_host_is_representable_but_invalid() {
    let delivery: ProviderDelivery = serde_json::from_value(json!({"mode": "via"})).unwrap();
    assert!(delivery.is_via());
    assert_eq!(delivery.via_host_id, None);
    assert!(!delivery.is_valid());

    assert!(ProviderDelivery::direct().is_valid());
    assert!(ProviderDelivery::via(host_id(), ApiRouteMode::Auto).is_valid());
}

/// A `route` on a direct delivery is a value the operator set, so the field is
/// not the omitted default and must survive the round trip.
#[test]
fn explicit_direct_with_a_route_is_not_the_omitted_default() {
    let delivery = ProviderDelivery {
        mode: ProviderDeliveryMode::Direct,
        via_host_id: None,
        route: ApiRouteMode::HubRelay,
    };
    assert!(!delivery.is_direct_default());
    let value = serde_json::to_value(&delivery).unwrap();
    assert_eq!(value, json!({"mode": "direct", "route": "hub-relay"}));
    assert_eq!(
        serde_json::from_value::<ProviderDelivery>(value).unwrap(),
        delivery
    );
    assert!(ProviderDelivery::direct().is_direct_default());
}

// ── ApiRoute / RequestedApiRoute ───────────────────────────────────────────

/// The echoed route is an observation: `auto` is not a value it can hold.
#[test]
fn api_route_echoes_only_resolved_routes() {
    let direct = ApiRoute::direct();
    assert_eq!(direct.mode, ProviderDeliveryMode::Direct);
    assert_eq!(direct.route, None);
    assert!(!direct.is_via());
    assert_eq!(
        serde_json::to_value(&direct).unwrap(),
        json!({"mode": "direct"})
    );

    for (kind, wire) in [
        (ApiRouteKind::DirectNet, "direct-net"),
        (ApiRouteKind::HubRelay, "hub-relay"),
    ] {
        let route = ApiRoute::via(host_id(), Some("mac-host".into()), kind);
        assert!(route.is_via());
        assert_eq!(route.route, Some(kind));
        assert_eq!(kind.as_str(), wire);
        let value = serde_json::to_value(&route).unwrap();
        assert_eq!(value["route"], json!(wire));
        assert_eq!(value["viaHostId"], json!(host_id().as_id().as_str()));
        assert_eq!(value["viaHostLabel"], json!("mac-host"));
        assert_eq!(serde_json::from_value::<ApiRoute>(value).unwrap(), route);
    }

    // `auto` is a request, never an observation (D-035 rule 4).
    assert!(serde_json::from_value::<ApiRoute>(json!({"mode": "via", "route": "auto"})).is_err());
    assert!(serde_json::from_value::<ApiRouteKind>(json!("auto")).is_err());
}

#[test]
fn requested_api_route_carries_intent_including_auto() {
    let route: RequestedApiRoute =
        serde_json::from_value(json!({"mode": "via", "route": "auto"})).unwrap();
    assert_eq!(route.mode, ProviderDeliveryMode::Via);
    assert_eq!(route.route, ApiRouteMode::Auto);
    assert_eq!(route.via_host_id, None);

    let plain: RequestedApiRoute = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        plain,
        RequestedApiRoute {
            mode: ProviderDeliveryMode::Direct,
            via_host_id: None,
            route: ApiRouteMode::Auto,
        }
    );
}

// ── refusals (§B.5) ────────────────────────────────────────────────────────

/// Every refusal is refused, and none of them is a fallback.
#[test]
fn every_via_refusal_has_a_code_and_a_status() {
    assert_eq!(
        ApiViaRefusal::ApiViaUnknownHost.as_str(),
        "api-via-unknown-host"
    );
    assert_eq!(
        ApiViaRefusal::ApiViaHostOffline.as_str(),
        "api-via-host-offline"
    );
    assert_eq!(
        ApiViaRefusal::ApiViaUnsupported.as_str(),
        "api-via-unsupported"
    );
    assert_eq!(
        ApiViaRefusal::ApiViaUnreachable.as_str(),
        "api-via-unreachable"
    );
    // 400 for a name that does not exist; 409 for state that may clear.
    assert_eq!(ApiViaRefusal::ApiViaUnknownHost.status(), 400);
    for conflict in [
        ApiViaRefusal::ApiViaHostOffline,
        ApiViaRefusal::ApiViaUnsupported,
        ApiViaRefusal::ApiViaUnreachable,
    ] {
        assert_eq!(conflict.status(), 409, "{conflict:?}");
    }
    for refusal in [
        ApiViaRefusal::ApiViaUnknownHost,
        ApiViaRefusal::ApiViaHostOffline,
        ApiViaRefusal::ApiViaUnsupported,
        ApiViaRefusal::ApiViaUnreachable,
    ] {
        let value = serde_json::to_value(refusal).unwrap();
        assert_eq!(value, json!(refusal.as_str()));
        assert_eq!(
            serde_json::from_value::<ApiViaRefusal>(value).unwrap(),
            refusal
        );
    }
    assert_eq!(API_ROUTE_DOWN, "api-route-down");
}

// ── host relay bind (Amendment A1) ─────────────────────────────────────────

/// A host with no relay bind keeps its default shape: absent, meaning the
/// listener stays loopback-only and every `via` session takes `hub-relay`.
#[test]
fn host_without_a_relay_bind_is_unchanged_and_absent() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host.json");
    let value: Value = from_json_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(value.get("relayBind").is_none());
    let host: Host = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(host.relay_bind, None);
    assert_eq!(serde_json::to_value(&host).unwrap(), value);
}

#[test]
fn host_relay_bind_round_trips() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host.json");
    let mut value: Value = from_json_slice(&std::fs::read(path).unwrap()).unwrap();
    value["relayBind"] = json!({
        "addr": "10.0.0.4:8899",
        "allowFrom": ["10.0.0.0/24"],
    });
    let host: Host = serde_json::from_value(value.clone()).unwrap();
    let bind = host.relay_bind.clone().expect("relayBind");
    assert_eq!(bind.addr, "10.0.0.4:8899");
    assert_eq!(bind.allow_from, vec!["10.0.0.0/24".to_string()]);
    assert_eq!(serde_json::to_value(&host).unwrap(), value);

    // `allowFrom` is optional; an empty list is a real value, not a wildcard.
    let minimal: HostRelayBind = serde_json::from_value(json!({"addr": "10.0.0.4:8899"})).unwrap();
    assert!(minimal.allow_from.is_empty());
}

// ── per-dispatch override (D-047) ──────────────────────────────────────────

/// The per-dispatch override is a string with two keywords and one id. Every
/// spelling the CLI accepts parses, and nothing else does.
#[test]
fn api_via_override_parses_host_self_and_none() {
    let host = host_id();
    for (wire, expected) in [
        (host.as_id().as_str(), ApiViaOverride::Host(host.clone())),
        ("self", ApiViaOverride::HubHost),
        ("none", ApiViaOverride::Direct),
        // Surrounding whitespace is a paste artefact, not a different value.
        ("  self  ", ApiViaOverride::HubHost),
        (" none", ApiViaOverride::Direct),
    ] {
        let parsed = ApiViaOverride::parse(wire).expect(wire);
        assert_eq!(parsed, expected, "{wire}");
        assert_eq!(parsed.as_wire(), expected.as_wire());
        assert_eq!(wire.trim().parse::<ApiViaOverride>().unwrap(), expected);
        assert_eq!(parsed.to_string(), expected.as_wire());
    }

    // A host id must be a real `hst_…`; `self`/`none` are the only keywords.
    for bad in [
        "",
        "selfie",
        "nonexistent",
        "self:",
        "ins_01993ab0-0000-7000-8000-000000000001",
    ] {
        assert!(
            ApiViaOverride::parse(bad).is_err(),
            "{bad:?} must not parse as an override"
        );
    }
}

/// `self`, `none` and an id must stay distinguishable, because `none` forces
/// direct and a mistyped id must not silently do the same.
#[test]
fn api_via_override_wire_values_are_distinct() {
    let host = host_id();
    let values = [
        ApiViaOverride::Host(host.clone()).as_wire(),
        ApiViaOverride::HubHost.as_wire(),
        ApiViaOverride::Direct.as_wire(),
    ];
    assert_eq!(values[1], "self");
    assert_eq!(values[2], "none");
    assert_eq!(values[0], host.as_id().as_str());
    let unique: std::collections::BTreeSet<_> = values.iter().collect();
    assert_eq!(unique.len(), 3);
}

/// The override is a string on the wire, so it also round-trips as one.
#[test]
fn api_via_override_serializes_as_a_plain_string() {
    for override_ in [
        ApiViaOverride::Host(host_id()),
        ApiViaOverride::HubHost,
        ApiViaOverride::Direct,
    ] {
        let value = serde_json::to_value(&override_).unwrap();
        assert!(value.is_string(), "{override_:?} must be a bare string");
        assert_eq!(value, json!(override_.as_wire()));
        assert_eq!(
            serde_json::from_value::<ApiViaOverride>(value).unwrap(),
            override_
        );
    }
}
