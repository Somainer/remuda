//! D-021 Claude provider resolution: request → host binding → scoped default → native.
//!
//! D-047 additionally resolves the model-API delivery waterfall
//! (request `apiVia` > project > profile `delivery` > direct) into a
//! [`ApiRouteChoice`]. The waterfall itself is a pure function — registry,
//! liveness and capability checks live in `providers.rs` — so every refusal
//! rule has a unit test without a Hub.

use crate::error::HubError;
use crate::store::{HostRecord, ProviderRecord};
use remuda_protocol::{ApiRouteMode, ApiViaOverride, HostRelayBind, ProviderDelivery};
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
/// Project default provider, between explicit and host binding; design §6.
pub const SOURCE_PROJECT: &str = "project-default";
/// The route came from the profile's own `delivery` (D-047 waterfall).
pub const SOURCE_PROFILE_DELIVERY: &str = "profile-delivery";

// ── D-047 delivery waterfall ────────────────────────────────────────────────

/// Where a proxied session's model API egresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViaTarget {
    /// `apiVia: "self"` — the Hub process itself is the egress host.
    HubHost,
    /// An enrolled host's Node is the egress host.
    Host(String),
}

/// One waterfall layer's delivery say (request or project).
#[derive(Clone, Debug)]
pub struct ApiViaLayer {
    /// `<hostId>` | `self` | `none`.
    pub via: ApiViaOverride,
    /// Optional sub-mode; absent means "fall through to the profile's route".
    pub route: Option<ApiRouteMode>,
}

/// The waterfall's `via` answer: this session proxies through [`Self::target`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiRouteChoice {
    /// Egress host.
    pub target: ViaTarget,
    /// Requested sub-mode. `auto` survives here; the launch-time resolver in
    /// `providers.rs` downgrades it to `hub-relay` when the target has no
    /// relay bind.
    pub route: ApiRouteMode,
    /// Waterfall step that named this route.
    pub source: &'static str,
}

/// The waterfall's answer.
///
/// A layer's explicit say suppresses every lower layer even when that say is
/// direct (`none`): without that, a profile-level `via` would resurrect behind
/// a request that forced direct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteAnswer {
    /// This session proxies through [`ApiRouteChoice::target`].
    Via(ApiRouteChoice),
    /// A layer explicitly forced direct; lower layers must not revive `via`.
    Direct,
}

/// One explicit (request or project) layer's resolved say.
#[must_use]
pub fn resolve_api_route(
    worker_host_id: &str,
    request: Option<&ApiViaLayer>,
    project: Option<&ApiViaLayer>,
    profile: &ProviderDelivery,
) -> Option<ApiRouteChoice> {
    if let Some(layer) = request
        && let Some(answer) = apply_layer(layer, profile.route, worker_host_id, SOURCE_REQUEST)
    {
        return answer.into_choice();
    }
    if let Some(layer) = project
        && let Some(answer) = apply_layer(layer, profile.route, worker_host_id, SOURCE_PROJECT)
    {
        return answer.into_choice();
    }
    if profile.is_via() {
        // Deserialization already guarantees a host id on `via`; the wire
        // type's hand-written Deserialize rejects via-without-host.
        if let Some(id) = profile.via_host_id.as_ref()
            && id.as_id().as_str() != worker_host_id
        {
            return Some(ApiRouteChoice {
                target: ViaTarget::Host(id.as_id().as_str().to_owned()),
                route: profile.route,
                source: SOURCE_PROFILE_DELIVERY,
            });
        }
    }
    None
}

/// Apply one explicit (request/project) layer. `None` means the layer did not
/// speak (it should fall through); a present answer is final for the
/// waterfall.
fn apply_layer(
    layer: &ApiViaLayer,
    profile_route: ApiRouteMode,
    worker_host_id: &str,
    source: &'static str,
) -> Option<RouteAnswer> {
    match &layer.via {
        // An explicit `none` is a decision, not an absence: it forces direct
        // over the project and profile layers.
        ApiViaOverride::Direct => Some(RouteAnswer::Direct),
        ApiViaOverride::HubHost => Some(RouteAnswer::Via(ApiRouteChoice {
            target: ViaTarget::HubHost,
            route: layer.route.unwrap_or(profile_route),
            source,
        })),
        ApiViaOverride::Host(id) if id.as_id().as_str() == worker_host_id => {
            // Naming the worker host itself collapses to direct at the
            // decision point — nothing to proxy.
            Some(RouteAnswer::Direct)
        }
        ApiViaOverride::Host(id) => Some(RouteAnswer::Via(ApiRouteChoice {
            target: ViaTarget::Host(id.as_id().as_str().to_owned()),
            route: layer.route.unwrap_or(profile_route),
            source,
        })),
    }
}

impl RouteAnswer {
    fn into_choice(self) -> Option<ApiRouteChoice> {
        match self {
            Self::Via(choice) => Some(choice),
            Self::Direct => None,
        }
    }
}

/// Whether a connected Node advertised the D-048 relay stream class.
///
/// Absence means "cannot": a host that never says it speaks `api.*` is refused
/// with `api-via-unsupported` rather than having a launch fail at first
/// request, and never silently downgraded to direct.
#[must_use]
pub fn node_supports_api_relay(host: &HostRecord) -> bool {
    let caps = &host.capabilities;
    if caps.get("apiRelay").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    caps.get("features")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str() == Some("api-relay-v1"))
        })
}

/// Validate an operator-supplied relay bind (D-047 Amendment A1).
///
/// The bind is an explicit address a listener opens on, so the bar is the one
/// the decision sets: no wildcard, no public address. Loopback and private /
/// link-local ranges are what the feature exists for. `allowFrom` entries must
/// each be a literal IP or an `ip/prefix` CIDR.
pub fn validate_relay_bind(bind: &HostRelayBind) -> Result<(), String> {
    let (host, port) = bind
        .addr
        .rsplit_once(':')
        .ok_or_else(|| format!("relayBind.addr must be host:port, got {}", bind.addr))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|_| format!("relayBind.addr host must be an IP literal, got {host}"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("relayBind.addr port must be 0..=65535, got {port}"))?;
    if port == 0 {
        return Err("relayBind.addr port must be non-zero".into());
    }
    match ip {
        std::net::IpAddr::V4(ip) => {
            if ip.is_unspecified() {
                return Err(
                    "relayBind.addr must not be 0.0.0.0; name a loopback or private address".into(),
                );
            }
            if !is_private_or_loopback_v4(ip) {
                return Err(
                    "relayBind.addr must be a loopback or private address, not a public one".into(),
                );
            }
        }
        std::net::IpAddr::V6(ip) => {
            if ip.is_unspecified() {
                return Err(
                    "relayBind.addr must not be ::; name a loopback or private address".into(),
                );
            }
            if !(ip.is_loopback() || is_private_v6(ip)) {
                return Err(
                    "relayBind.addr must be a loopback or private address, not a public one".into(),
                );
            }
        }
    }
    for entry in &bind.allow_from {
        validate_allow_from(entry)?;
    }
    Ok(())
}

fn is_private_or_loopback_v4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_private_v6(ip: std::net::Ipv6Addr) -> bool {
    // Unique local addresses fc00::/7 plus fe80::/10 link-local.
    let seg = ip.segments()[0];
    (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80
}

fn validate_allow_from(entry: &str) -> Result<(), String> {
    let (addr, prefix) = match entry.rsplit_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (entry, None),
    };
    let ip: std::net::IpAddr = addr
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| format!("allowFrom entry must be an IP or CIDR, got {entry}"))?;
    if let Some(prefix) = prefix {
        let prefix: u32 = prefix
            .parse()
            .map_err(|_| format!("allowFrom CIDR prefix must be a number, got {entry}"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("allowFrom CIDR prefix /{prefix} exceeds /{max}"));
        }
    }
    Ok(())
}

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
    /// Project default profile id; wins over host bindings but loses to the
    /// explicit request (design §6).
    pub project_profile_id: Option<&'a str>,
    /// Project default delegation (`gateway` / `direct`).
    pub project_delegation: Option<&'a str>,
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

    // Project layer: explicit > project > host > global (design §6).
    let project_delegation = input
        .project_delegation
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let project_profile = input
        .project_profile_id
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if !explicit_provider {
        if let Some(id) = project_profile.filter(|id| is_real_profile_id(id)) {
            return match find_profile(input.profiles, id) {
                Some(profile) if profile_allowed_on_host(&profile.scope, host_id) => {
                    Ok(ResolvedProvider::Profile {
                        profile: Box::new(profile.clone()),
                        source: SOURCE_PROJECT,
                    })
                }
                Some(profile) => Err(HubError::Unsatisfiable {
                    reasons: vec![format!(
                        "{host_id}: project profile {} is bound to {}",
                        profile.id, profile.scope
                    )],
                }),
                None => Err(HubError::Unsatisfiable {
                    reasons: vec![format!("project provider profile {id} is missing")],
                }),
            };
        }
        if matches!(project_delegation, Some("none")) {
            return Ok(ResolvedProvider::Native {
                source: SOURCE_PROJECT,
            });
        }
        // A project that pins gateway/direct suppresses the host binding: the
        // project layer sits strictly above it.
        if project_delegation.is_none() {
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
                            reasons: vec![format!(
                                "{host_id}: host binding profile {id} is missing"
                            )],
                        }),
                    };
                }
                Binding::Auto => {}
            }
        }
    }

    let default_source = if project_delegation.is_some() {
        SOURCE_PROJECT
    } else {
        SOURCE_HOST_SCOPED_DEFAULT
    };
    let host_scope = format!("host:{host_id}");
    if let Some(profile) = default_gateway_in(input.profiles, &host_scope) {
        return Ok(ResolvedProvider::Profile {
            profile: Box::new(profile.clone()),
            source: default_source,
        });
    }
    let universal_source = if project_delegation.is_some() {
        SOURCE_PROJECT
    } else {
        SOURCE_UNIVERSAL_DEFAULT
    };
    if let Some(profile) = default_gateway_in(input.profiles, "universal") {
        return Ok(ResolvedProvider::Profile {
            profile: Box::new(profile.clone()),
            source: universal_source,
        });
    }
    if explicit_gateway || project_delegation == Some("gateway") {
        return Err(HubError::ProviderNotConfigured {
            reasons: vec![format!(
                "no gateway provider configured for host {host_id}; add a provider or choose native"
            )],
        });
    }
    if !explicit_provider && project_delegation.is_none() && host_reports_native_claude(input.host)
    {
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
            // A `native` profile is a named account row the Hub holds *no* token
            // for: the CLI on the host uses its own login. So it delegates to
            // nothing, exactly like the alias forms above — the profile id is
            // still recorded (supply admitted it as a pin, and its model ids are
            // the reason it exists), but no launch secret is ever demanded for
            // it. Classifying it as `gateway` is what made dispatch fail with
            // "provider profile has no stored auth token" for a profile that by
            // design has none (docs/design/evidence/dispatch-driver-1.md).
            let delegation = match profile.kind.as_str() {
                "direct" => "direct",
                "native" => "none",
                _ => "gateway",
            };
            let model = obj.get("model").and_then(Value::as_str).map(str::to_string);
            obj.insert("delegation".into(), json!(delegation));
            obj.insert("providerProfileId".into(), json!(profile.id));
            obj.insert("providerScope".into(), json!(profile.scope));
            if profile.kind == "native" {
                // No gateway to point at and no token to carry: an overlay here
                // would only describe a base URL the native login does not use.
                obj.remove("providerOverlay");
            } else {
                obj.insert(
                    "providerOverlay".into(),
                    profile.overlay_spec(model.as_deref()),
                );
            }
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
            host_os: None,
            provider_binding: binding.into(),
            default_launch_args: None,
            claude_binary_path: None,
            default_tui: None,
            relay_bind: None,
        }
    }

    fn profile(id: &str, name: &str, scope: &str, default: bool) -> ProviderRecord {
        ProviderRecord {
            id: id.into(),
            name: name.into(),
            kind: "gateway".into(),
            base_url: "http://127.0.0.1:1".into(),
            models: vec![crate::provider_models::ProviderModel::plain("m")],
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
            supply: Default::default(),
            delivery: Default::default(),
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
            project_profile_id: None,
            project_delegation: None,
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

    // ── D-047 delivery waterfall ──────────────────────────────────────────

    fn via_profile(id: &remuda_protocol::HostId, route: ApiRouteMode) -> ProviderDelivery {
        ProviderDelivery::via(id.clone(), route)
    }

    fn layer(via: ApiViaOverride, route: Option<ApiRouteMode>) -> ApiViaLayer {
        ApiViaLayer { via, route }
    }

    fn host_override(id: &remuda_protocol::HostId) -> ApiViaOverride {
        ApiViaOverride::Host(id.clone())
    }

    fn host_variant(id: &remuda_protocol::HostId) -> ViaTarget {
        ViaTarget::Host(id.as_id().as_str().to_string())
    }

    #[test]
    fn waterfall_defaults_to_direct() {
        let worker = remuda_protocol::HostId::new();
        let choice = resolve_api_route(
            worker.as_id().as_str(),
            None,
            None,
            &ProviderDelivery::direct(),
        );
        assert_eq!(choice, None);
    }

    #[test]
    fn profile_via_is_used_when_no_explicit_layer_speaks() {
        let worker = remuda_protocol::HostId::new();
        let proxy = remuda_protocol::HostId::new();
        let delivery = via_profile(&proxy, ApiRouteMode::HubRelay);
        let choice =
            resolve_api_route(worker.as_id().as_str(), None, None, &delivery).expect("via");
        assert_eq!(choice.target, host_variant(&proxy));
        assert_eq!(choice.route, ApiRouteMode::HubRelay);
        assert_eq!(choice.source, SOURCE_PROFILE_DELIVERY);
    }

    #[test]
    fn request_layer_wins_over_project_and_profile() {
        let worker = remuda_protocol::HostId::new();
        let from_profile = remuda_protocol::HostId::new();
        let from_project = remuda_protocol::HostId::new();
        let from_request = remuda_protocol::HostId::new();
        let delivery = via_profile(&from_profile, ApiRouteMode::HubRelay);
        let request = layer(host_override(&from_request), None);
        let project = layer(host_override(&from_project), None);
        let choice = resolve_api_route(
            worker.as_id().as_str(),
            Some(&request),
            Some(&project),
            &delivery,
        )
        .expect("via");
        assert_eq!(choice.target, host_variant(&from_request));
        // Route falls through to the profile when the layer names no sub-mode.
        assert_eq!(choice.route, ApiRouteMode::HubRelay);
        assert_eq!(choice.source, SOURCE_REQUEST);
    }

    #[test]
    fn project_layer_wins_over_profile_delivery() {
        let worker = remuda_protocol::HostId::new();
        let from_profile = remuda_protocol::HostId::new();
        let from_project = remuda_protocol::HostId::new();
        let delivery = via_profile(&from_profile, ApiRouteMode::Auto);
        let project = layer(host_override(&from_project), Some(ApiRouteMode::DirectNet));
        let choice = resolve_api_route(worker.as_id().as_str(), None, Some(&project), &delivery)
            .expect("via");
        assert_eq!(choice.target, host_variant(&from_project));
        assert_eq!(choice.route, ApiRouteMode::DirectNet);
        assert_eq!(choice.source, SOURCE_PROJECT);
    }

    #[test]
    fn request_none_forces_direct_even_when_profile_and_project_say_via() {
        let worker = remuda_protocol::HostId::new();
        let from_profile = remuda_protocol::HostId::new();
        let from_project = remuda_protocol::HostId::new();
        let delivery = via_profile(&from_profile, ApiRouteMode::HubRelay);
        let request = layer(ApiViaOverride::Direct, Some(ApiRouteMode::HubRelay));
        let project = layer(host_override(&from_project), None);
        let choice = resolve_api_route(
            worker.as_id().as_str(),
            Some(&request),
            Some(&project),
            &delivery,
        );
        assert_eq!(choice, None, "none forces direct over every lower layer");
    }

    #[test]
    fn project_none_forces_direct_over_profile() {
        let worker = remuda_protocol::HostId::new();
        let from_profile = remuda_protocol::HostId::new();
        let delivery = via_profile(&from_profile, ApiRouteMode::HubRelay);
        let project = layer(ApiViaOverride::Direct, None);
        let choice = resolve_api_route(worker.as_id().as_str(), None, Some(&project), &delivery);
        assert_eq!(choice, None);
    }

    #[test]
    fn via_the_worker_host_collapses_to_direct_in_every_layer() {
        let worker = remuda_protocol::HostId::new();
        let delivery = via_profile(&worker, ApiRouteMode::HubRelay);
        assert_eq!(
            resolve_api_route(worker.as_id().as_str(), None, None, &delivery),
            None
        );
        let request = layer(host_override(&worker), None);
        assert_eq!(
            resolve_api_route(worker.as_id().as_str(), Some(&request), None, &delivery),
            None
        );
        let project = layer(host_override(&worker), None);
        assert_eq!(
            resolve_api_route(
                worker.as_id().as_str(),
                None,
                Some(&project),
                &ProviderDelivery::direct()
            ),
            None
        );
    }

    #[test]
    fn via_self_targets_the_hub_host_and_never_collapses() {
        let worker = remuda_protocol::HostId::new();
        let choice = resolve_api_route(
            worker.as_id().as_str(),
            Some(&layer(ApiViaOverride::HubHost, None)),
            None,
            &ProviderDelivery::direct(),
        )
        .expect("self proxies");
        assert_eq!(choice.target, ViaTarget::HubHost);
        assert_eq!(choice.route, ApiRouteMode::Auto);
        assert_eq!(choice.source, SOURCE_REQUEST);
    }

    #[test]
    fn request_route_overrides_profile_route() {
        let worker = remuda_protocol::HostId::new();
        let proxy = remuda_protocol::HostId::new();
        let delivery = via_profile(&proxy, ApiRouteMode::HubRelay);
        let request = layer(host_override(&proxy), Some(ApiRouteMode::DirectNet));
        let choice = resolve_api_route(worker.as_id().as_str(), Some(&request), None, &delivery)
            .expect("via");
        assert_eq!(choice.route, ApiRouteMode::DirectNet);
    }

    #[test]
    fn relay_capability_is_read_from_either_hello_shape() {
        let mut host = host_rec("hst_h", "auto", false);
        host.capabilities = json!({});
        assert!(!node_supports_api_relay(&host));
        host.capabilities = json!({ "apiRelay": true });
        assert!(node_supports_api_relay(&host));
        host.capabilities = json!({ "features": ["tty-v9", "api-relay-v1"] });
        assert!(node_supports_api_relay(&host));
        host.capabilities = json!({ "apiRelay": false, "features": [] });
        assert!(!node_supports_api_relay(&host));
    }

    #[test]
    fn relay_bind_validation_rejects_wildcards_and_public_addresses() {
        let bind = |addr: &str| HostRelayBind {
            addr: addr.into(),
            allow_from: vec![],
        };
        assert!(validate_relay_bind(&bind("0.0.0.0:8443")).is_err());
        assert!(validate_relay_bind(&bind("[::]:8443")).is_err());
        assert!(validate_relay_bind(&bind("8.8.8.8:8443")).is_err());
        assert!(validate_relay_bind(&bind("2001:4860:4860::8888:8443")).is_err());
        assert!(validate_relay_bind(&bind("127.0.0.1:0")).is_err());
        assert!(validate_relay_bind(&bind("not-an-ip:8443")).is_err());
        assert!(validate_relay_bind(&bind("127.0.0.1:8443")).is_ok());
        assert!(validate_relay_bind(&bind("192.168.1.10:8443")).is_ok());
        assert!(validate_relay_bind(&bind("[fd00::1]:8443")).is_ok());
        let with_allow = remuda_protocol::HostRelayBind {
            addr: "10.0.0.1:8443".into(),
            allow_from: vec!["10.0.0.0/8".into(), "fe80::1".into()],
        };
        assert!(validate_relay_bind(&with_allow).is_ok());
        let bad_allow = remuda_protocol::HostRelayBind {
            addr: "10.0.0.1:8443".into(),
            allow_from: vec!["not-a-cidr".into()],
        };
        assert!(validate_relay_bind(&bad_allow).is_err());
    }
}
