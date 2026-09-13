//! D-021 Claude provider resolution: request → host binding → scoped default → native.

use crate::error::HubError;
use crate::store::{HostRecord, ProviderRecord};
use serde_json::{Value, json};

/// Explicit request (profile id or `delegation`).
pub const SOURCE_REQUEST: &str = "request";
/// Host `providerBinding` of `native` or `profile:<id>`.
pub const SOURCE_HOST_BINDING: &str = "host-binding";
/// Default gateway in `host:<hostId>` scope.
pub const SOURCE_HOST_SCOPED_DEFAULT: &str = "host-scoped-default";
/// Default gateway in `universal` scope.
pub const SOURCE_UNIVERSAL_DEFAULT: &str = "universal-default";
/// Host CLI inventory reported a native login or gateway.
pub const SOURCE_HOST_INVENTORY: &str = "host-inventory";
/// No default profile matched; let the host determine whether native auth works.
pub const SOURCE_NATIVE_FALLBACK: &str = "native-fallback";

/// Chosen Claude provider for one host.
#[derive(Clone, Debug)]
pub enum ResolvedProvider {
    /// Inherit the host CLI login; Hub does not inject a token.
    Native {
        /// Waterfall step that selected native.
        source: &'static str,
    },
    /// Hub-stored profile delivered through SecretBroker.
    Profile {
        /// Registry row (token stays in the vault).
        profile: Box<ProviderRecord>,
        /// Waterfall step that selected this profile.
        source: &'static str,
    },
}

impl ResolvedProvider {
    /// Waterfall step name stored on the instance spec.
    pub fn source(&self) -> &'static str {
        match self {
            Self::Native { source } | Self::Profile { source, .. } => source,
        }
    }

    /// Operator-facing line for Session header / New Session hint.
    pub fn hint(&self) -> String {
        match self {
            Self::Native {
                source: SOURCE_NATIVE_FALLBACK,
            } => "未匹配到默认供应商配置，将尝试使用主机原生认证；认证是否可用由主机确认".into(),
            Self::Native { .. } => "使用主机原生登录".into(),
            Self::Profile { profile, .. } => {
                let label = scope_label(&profile.scope);
                format!("将使用 {} ({label})", profile.name)
            }
        }
    }
}

/// Inputs for the Hub-side waterfall.
pub struct ResolveInput<'a> {
    /// Chosen host (binding + CLI inventory).
    pub host: &'a HostRecord,
    /// All stored profiles (caller may already have filtered).
    pub profiles: &'a [ProviderRecord],
    /// Explicit `delegation` from the create body.
    pub delegation: Option<&'a str>,
    /// Explicit `providerProfileId` from the create body.
    pub provider_profile_id: Option<&'a str>,
}

/// `universal` or `host:<hostId>`.
pub fn normalize_scope(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "universal" {
        return Ok("universal".into());
    }
    if let Some(host_id) = raw.strip_prefix("host:") {
        let host_id = host_id.trim();
        if host_id.is_empty() {
            return Err("scope host:<hostId> is missing a host id".into());
        }
        return Ok(format!("host:{host_id}"));
    }
    Err("scope must be universal or host:<hostId>".into())
}

/// `auto` | `native` | `profile:<id>`.
pub fn normalize_binding(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "auto" {
        return Ok("auto".into());
    }
    if raw == "native" {
        return Ok("native".into());
    }
    if let Some(id) = raw.strip_prefix("profile:") {
        let id = id.trim();
        if !is_real_profile_id(id) {
            return Err("providerBinding profile:<id> needs a stored profile id".into());
        }
        return Ok(format!("profile:{id}"));
    }
    Err("providerBinding must be auto, native, or profile:<id>".into())
}

/// True when a stored profile may run on `host_id`.
pub fn profile_allowed_on_host(scope: &str, host_id: &str) -> bool {
    let scope = scope.trim();
    if scope.is_empty() || scope == "universal" {
        return true;
    }
    scope
        .strip_prefix("host:")
        .is_some_and(|bound| bound == host_id)
}

/// SecretBroker gate: host-scoped tokens are only released to that host.
pub fn secret_release_allowed(profile: &ProviderRecord, host_id: &str) -> bool {
    profile_allowed_on_host(&profile.scope, host_id)
}

/// Real `pvp_…` id, not a D-012 alias (`none` / `native` / `native-login` / `gateway` / …).
pub fn is_real_profile_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id != "none"
        && id != "native"
        && id != "native-login"
        && id != "gateway"
        && id != "direct"
        && id != "auto"
        && id != "host"
}

/// Claude CLI inventory reports a native login or API gateway (booleans only).
pub fn host_reports_native_claude(host: &HostRecord) -> bool {
    let Some(items) = host.cli.as_array() else {
        return false;
    };
    items.iter().any(|item| {
        if item.get("kind").and_then(Value::as_str) != Some("claude") {
            return false;
        }
        if item.get("nativeGateway").and_then(Value::as_bool) == Some(true) {
            return true;
        }
        matches!(
            item.get("auth").and_then(Value::as_str),
            Some("logged_in" | "gateway-native")
        )
    })
}

/// Resolve a Claude launch, distinguishing missing gateways from placement failures.
pub fn resolve(input: ResolveInput<'_>) -> Result<ResolvedProvider, HubError> {
    let host_id = input.host.host_id.as_str();
    let requested = input
        .provider_profile_id
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let delegation = input.delegation.map(str::trim).filter(|s| !s.is_empty());
    let explicit_gateway =
        delegation == Some("gateway") || delegation.is_none() && requested == Some("gateway");
    let explicit_provider = explicit_gateway || delegation == Some("direct");
    let explicit_native = matches!(delegation, Some("none"))
        || matches!(requested, Some("none" | "native" | "native-login")) && !explicit_provider;

    if let Some(id) = requested.filter(|id| is_real_profile_id(id)) {
        return match find_profile(input.profiles, id) {
            Some(profile) if profile_allowed_on_host(&profile.scope, host_id) => {
                Ok(ResolvedProvider::Profile {
                    profile: Box::new(profile.clone()),
                    source: SOURCE_REQUEST,
                })
            }
            Some(profile) => Err(HubError::Unsatisfiable {
                reasons: vec![format!(
                    "{host_id}: host-scoped profile {} is bound to {}",
                    profile.id, profile.scope
                )],
            }),
            None => Err(HubError::Unsatisfiable {
                reasons: vec![format!("unknown provider profile {id}")],
            }),
        };
    }

    if explicit_native {
        return Ok(ResolvedProvider::Native {
            source: SOURCE_REQUEST,
        });
    }

    if !explicit_provider {
        match binding_of(&input.host.provider_binding) {
            Binding::Native => {
                return Ok(ResolvedProvider::Native {
                    source: SOURCE_HOST_BINDING,
                });
            }
            Binding::Profile(id) => {
                return match find_profile(input.profiles, &id) {
                    Some(profile) if profile_allowed_on_host(&profile.scope, host_id) => {
                        Ok(ResolvedProvider::Profile {
                            profile: Box::new(profile.clone()),
                            source: SOURCE_HOST_BINDING,
                        })
                    }
                    Some(profile) => Err(HubError::Unsatisfiable {
                        reasons: vec![format!(
                            "{host_id}: host binding profile {} is bound to {}",
                            profile.id, profile.scope
                        )],
                    }),
                    None => Err(HubError::Unsatisfiable {
                        reasons: vec![format!("{host_id}: host binding profile {id} is missing")],
                    }),
                };
            }
            Binding::Auto => {}
        }
    }

    let host_scope = format!("host:{host_id}");
    if let Some(profile) = default_gateway_in(input.profiles, &host_scope) {
        return Ok(ResolvedProvider::Profile {
            profile: Box::new(profile.clone()),
            source: SOURCE_HOST_SCOPED_DEFAULT,
        });
    }
    if let Some(profile) = default_gateway_in(input.profiles, "universal") {
        return Ok(ResolvedProvider::Profile {
            profile: Box::new(profile.clone()),
            source: SOURCE_UNIVERSAL_DEFAULT,
        });
    }
    if explicit_gateway {
        return Err(HubError::ProviderNotConfigured {
            reasons: vec![format!(
                "no gateway provider configured for host {host_id}; add a provider or choose native"
            )],
        });
    }
    if !explicit_provider && host_reports_native_claude(input.host) {
        return Ok(ResolvedProvider::Native {
            source: SOURCE_HOST_INVENTORY,
        });
    }

    Ok(ResolvedProvider::Native {
        source: SOURCE_NATIVE_FALLBACK,
    })
}

/// Write the chosen source onto the instance spec (never the token).
pub fn apply_to_spec(spec: &mut Value, resolved: &ResolvedProvider) {
    let Some(obj) = spec.as_object_mut() else {
        return;
    };
    obj.insert("providerSource".into(), json!(resolved.source()));
    obj.insert("providerSourceHint".into(), json!(resolved.hint()));
    match resolved {
        ResolvedProvider::Native { .. } => {
            obj.insert("delegation".into(), json!("none"));
            obj.insert("providerProfileId".into(), json!("none"));
            obj.remove("providerOverlay");
        }
        ResolvedProvider::Profile { profile, .. } => {
            let delegation = if profile.kind == "direct" {
                "direct"
            } else {
                "gateway"
            };
            let model = obj.get("model").and_then(Value::as_str).map(str::to_string);
            obj.insert("delegation".into(), json!(delegation));
            obj.insert("providerProfileId".into(), json!(profile.id));
            obj.insert("providerScope".into(), json!(profile.scope));
            obj.insert(
                "providerOverlay".into(),
                profile.overlay_spec(model.as_deref()),
            );
        }
    }
}

fn scope_label(scope: &str) -> &'static str {
    if scope.starts_with("host:") {
        "host"
    } else {
        "universal"
    }
}

fn find_profile<'a>(profiles: &'a [ProviderRecord], id: &str) -> Option<&'a ProviderRecord> {
    profiles.iter().find(|profile| profile.id == id)
}

fn default_gateway_in<'a>(
    profiles: &'a [ProviderRecord],
    scope: &str,
) -> Option<&'a ProviderRecord> {
    profiles.iter().find(|profile| {
        profile.default_gateway
            && profile.kind == "gateway"
            && normalize_scope(&profile.scope).ok().as_deref() == Some(scope)
    })
}

enum Binding {
    Auto,
    Native,
    Profile(String),
}

fn binding_of(raw: &str) -> Binding {
    let normalized = normalize_binding(raw).unwrap_or_else(|_| "auto".into());
    if normalized == "native" {
        return Binding::Native;
    }
    if let Some(id) = normalized.strip_prefix("profile:") {
        return Binding::Profile(id.to_string());
    }
    Binding::Auto
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn host_rec(id: &str, binding: &str, native: bool) -> HostRecord {
        HostRecord {
            workspaces: Vec::new(),
            workspace_revision: 0,
            ssh: None,
            last_error: None,
            host_id: id.into(),
            label: id.into(),
            state: "online".into(),
            online: true,
            last_seen_at: None,
            node_version: None,
            cli: if native {
                json!([{
                    "kind": "claude",
                    "auth": "logged_in",
                    "installed": true,
                    "nativeGateway": false
                }])
            } else {
                json!([{ "kind": "claude", "auth": "logged_out", "installed": true }])
            },
            capabilities: json!({}),
            instance_count: 0,
            transport: "outbound-wss".into(),
            labels: Vec::new(),
            herdr: None,
            resources: None,
            max_instances: 8,
            hostname: None,
            provider_binding: binding.into(),
        }
    }

    fn profile(id: &str, name: &str, scope: &str, default: bool) -> ProviderRecord {
        ProviderRecord {
            id: id.into(),
            name: name.into(),
            kind: "gateway".into(),
            base_url: "http://127.0.0.1:1".into(),
            models: vec!["m".into()],
            default_model: Some("m".into()),
            headers: BTreeMap::new(),
            default_gateway: default,
            scope: scope.into(),
            revision: 1,
            secret_name: Some(format!("provider-{id}")),
            secret_present: true,
            secret_last4: Some("zzzz".into()),
            secret_fingerprint: Some("0123456789abcdef".into()),
            last_test_ok: None,
            last_test_at: None,
            last_test_message: None,
            created_at: "2026-09-13T00:00:00.000Z".into(),
            updated_at: "2026-09-13T00:00:00.000Z".into(),
        }
    }

    fn run(
        host: &HostRecord,
        profiles: &[ProviderRecord],
        delegation: Option<&str>,
        provider_profile_id: Option<&str>,
    ) -> Result<ResolvedProvider, HubError> {
        resolve(ResolveInput {
            host,
            profiles,
            delegation,
            provider_profile_id,
        })
    }

    #[test]
    fn explicit_profile_id_wins() {
        let host = host_rec("hst_a", "native", true);
        let profiles = vec![
            profile("pvp_u", "uni", "universal", true),
            profile("pvp_x", "explicit", "universal", false),
        ];
        let got = run(&host, &profiles, Some("none"), Some("pvp_x")).unwrap();
        match got {
            ResolvedProvider::Profile { profile, source } => {
                assert_eq!(profile.id, "pvp_x");
                assert_eq!(source, SOURCE_REQUEST);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn explicit_none_is_native() {
        let host = host_rec("hst_a", "profile:pvp_u", false);
        let profiles = vec![profile("pvp_u", "uni", "universal", true)];
        let got = run(&host, &profiles, Some("none"), Some("none")).unwrap();
        match got {
            ResolvedProvider::Native { source } => assert_eq!(source, SOURCE_REQUEST),
            other => panic!("{other:?}"),
        }
        match run(&host, &profiles, None, Some("native-login")).unwrap() {
            ResolvedProvider::Native { source } => assert_eq!(source, SOURCE_REQUEST),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn host_binding_native_and_profile() {
        let profiles = vec![profile("pvp_u", "uni", "universal", true)];
        let native = host_rec("hst_a", "native", false);
        match run(&native, &profiles, None, None).unwrap() {
            ResolvedProvider::Native { source } => assert_eq!(source, SOURCE_HOST_BINDING),
            other => panic!("{other:?}"),
        }
        let bound = host_rec("hst_a", "profile:pvp_u", false);
        match run(&bound, &profiles, None, None).unwrap() {
            ResolvedProvider::Profile { profile, source } => {
                assert_eq!(profile.id, "pvp_u");
                assert_eq!(source, SOURCE_HOST_BINDING);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn host_scoped_default_beats_universal() {
        let host = host_rec("hst_a", "auto", false);
        let profiles = vec![
            profile("pvp_u", "uni", "universal", true),
            profile("pvp_h", "hosty", "host:hst_a", true),
        ];
        match run(&host, &profiles, None, None).unwrap() {
            ResolvedProvider::Profile { profile, source } => {
                assert_eq!(profile.id, "pvp_h");
                assert_eq!(source, SOURCE_HOST_SCOPED_DEFAULT);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn universal_default_then_inventory_then_native_fallback() {
        let host = host_rec("hst_a", "auto", false);
        let uni = vec![profile("pvp_u", "uni", "universal", true)];
        match run(&host, &uni, None, None).unwrap() {
            ResolvedProvider::Profile { profile, source } => {
                assert_eq!(profile.id, "pvp_u");
                assert_eq!(source, SOURCE_UNIVERSAL_DEFAULT);
            }
            other => panic!("{other:?}"),
        }
        let native_host = host_rec("hst_a", "auto", true);
        match run(&native_host, &[], None, None).unwrap() {
            ResolvedProvider::Native { source } => assert_eq!(source, SOURCE_HOST_INVENTORY),
            other => panic!("{other:?}"),
        }
        let unmatched = vec![
            profile("pvp_other", "other-host", "host:hst_b", true),
            profile("pvp_nondefault", "nondefault", "universal", false),
        ];
        for cli in [
            Value::Null,
            json!([]),
            json!([{ "kind": "claude", "installed": true }]),
            json!([{ "kind": "claude", "auth": "unknown", "installed": true }]),
            host.cli.clone(),
        ] {
            let mut host = host.clone();
            host.cli = cli;
            for profiles in [&[][..], unmatched.as_slice()] {
                let got = run(&host, profiles, None, None).unwrap();
                assert!(matches!(
                    got,
                    ResolvedProvider::Native {
                        source: SOURCE_NATIVE_FALLBACK
                    }
                ));
                let mut spec = json!({"kind": "claude"});
                apply_to_spec(&mut spec, &got);
                assert_eq!(spec["delegation"], "none");
                assert_eq!(spec["providerProfileId"], "none");
                assert_eq!(spec["providerSource"], "native-fallback");
                assert_eq!(
                    spec["providerSourceHint"],
                    "未匹配到默认供应商配置，将尝试使用主机原生认证；认证是否可用由主机确认"
                );
            }
        }
    }

    #[test]
    fn explicit_gateway_requires_configuration_but_direct_uses_native_fallback() {
        for native in [false, true] {
            let host = host_rec("hst_a", "native", native);
            for (delegation, requested) in [
                (Some("gateway"), None),
                (Some("gateway"), Some("gateway")),
                (None, Some("gateway")),
            ] {
                let err = run(&host, &[], delegation, requested).unwrap_err();
                let HubError::ProviderNotConfigured { reasons } = err else {
                    panic!("{err:?}");
                };
                assert_eq!(
                    reasons,
                    [
                        "no gateway provider configured for host hst_a; add a provider or choose native"
                    ]
                );
            }
            let got = run(&host, &[], Some("direct"), None).unwrap();
            assert!(matches!(
                got,
                ResolvedProvider::Native {
                    source: SOURCE_NATIVE_FALLBACK
                }
            ));
            assert!(got.hint().contains("未匹配到默认供应商配置"));
        }
    }

    #[test]
    fn host_scoped_profile_is_refused_on_the_wrong_host() {
        let host = host_rec("hst_b", "auto", false);
        let profiles = vec![profile("pvp_h", "hosty", "host:hst_a", true)];
        let err = run(&host, &profiles, None, Some("pvp_h")).unwrap_err();
        let HubError::Unsatisfiable { reasons } = err else {
            panic!("{err:?}");
        };
        assert!(
            reasons.iter().any(|r| r.contains("bound to host:hst_a")),
            "{reasons:?}"
        );
        assert!(!secret_release_allowed(&profiles[0], "hst_b"));
        assert!(secret_release_allowed(&profiles[0], "hst_a"));
    }

    #[test]
    fn hint_strings() {
        let native = ResolvedProvider::Native {
            source: SOURCE_HOST_INVENTORY,
        };
        assert_eq!(native.hint(), "使用主机原生登录");
        let profile = ResolvedProvider::Profile {
            profile: Box::new(profile("pvp_u", "uni-gw", "universal", true)),
            source: SOURCE_UNIVERSAL_DEFAULT,
        };
        assert_eq!(profile.hint(), "将使用 uni-gw (universal)");
    }
}
