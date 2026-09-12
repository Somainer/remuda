//! Host selection: explicit host, label match, or least-loaded `any` (D-013).

use crate::AppState;
use crate::auth::{require_device, require_origin};
use crate::error::HubError;
use crate::store::{CommandRecord, HostRecord, InstanceRecord};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};

/// How to pick a host for an Instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Placement {
    /// Must be this host, and it must be online.
    Host {
        /// `hst_…`.
        host_id: String,
    },
    /// Online hosts whose labels contain every entry.
    Labels {
        /// Tags such as `region=sg` or `region:sg`.
        labels: Vec<String>,
    },
    /// Any online host that meets capability constraints.
    Any,
}

impl Placement {
    /// Parse `{kind:host|labels|any}` or a shorthand `{hostId}` / `{labels}` / `"any"`.
    pub fn from_value(value: Option<&Value>, host_id: Option<&str>) -> Result<Self, HubError> {
        if let Some(id) = host_id.filter(|s| !s.is_empty()) {
            return Ok(Self::Host {
                host_id: id.to_string(),
            });
        }
        let Some(value) = value else {
            return Ok(Self::Any);
        };
        if let Some(s) = value.as_str() {
            return match s {
                "any" => Ok(Self::Any),
                other => Ok(Self::Host {
                    host_id: other.to_string(),
                }),
            };
        }
        if let Some(id) = value.get("hostId").and_then(Value::as_str) {
            return Ok(Self::Host {
                host_id: id.to_string(),
            });
        }
        if let Some(id) = value.get("host").and_then(Value::as_str) {
            return Ok(Self::Host {
                host_id: id.to_string(),
            });
        }
        if let Some(labels) = value.get("labels").and_then(Value::as_array) {
            let labels: Vec<String> = labels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            return Ok(Self::Labels { labels });
        }
        match value.get("kind").and_then(Value::as_str) {
            Some("any") => Ok(Self::Any),
            Some("host") => {
                let host_id = value
                    .get("hostId")
                    .or_else(|| value.get("host"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| HubError::BadRequest("placement.host requires hostId".into()))?;
                Ok(Self::Host {
                    host_id: host_id.to_string(),
                })
            }
            Some("labels") => {
                let labels = value
                    .get("labels")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Self::Labels { labels })
            }
            Some(other) => Err(HubError::BadRequest(format!(
                "unknown placement kind {other}"
            ))),
            None => Ok(Self::Any),
        }
    }
}

/// Capability slice of an Instance spec used for placement.
#[derive(Clone, Debug, Default)]
pub struct PlaceSpec {
    /// Driver kind (`claude-print`, `claude-pty`, …).
    pub driver: String,
    /// `none` / `gateway` / `direct`.
    pub delegation: Option<String>,
}

impl PlaceSpec {
    /// Read driver + delegation from a JSON spec body.
    pub fn from_json(spec: &Value) -> Self {
        let driver = spec
            .get("driver")
            .and_then(Value::as_str)
            .unwrap_or("claude-print")
            .to_string();
        let delegation = spec
            .get("delegation")
            .and_then(Value::as_str)
            .or_else(|| spec.pointer("/profile/delegation").and_then(Value::as_str))
            .or_else(|| {
                spec.pointer("/providerProfile/delegation")
                    .and_then(Value::as_str)
            })
            .map(str::to_string);
        Self { driver, delegation }
    }
}

/// Ranked hosts that satisfy `placement` and `spec`, or an unsatisfiable error.
pub fn select_hosts(
    hosts: &[HostRecord],
    running: &[(String, i64)],
    placement: &Placement,
    spec: &PlaceSpec,
) -> Result<Vec<HostRecord>, HubError> {
    let mut reasons = Vec::new();
    let mut eligible = Vec::new();
    for host in hosts {
        match consider(host, running, placement, spec) {
            Ok(()) => eligible.push(host.clone()),
            Err(reason) => reasons.push(reason),
        }
    }
    if eligible.is_empty() {
        if reasons.is_empty() {
            reasons.push("no hosts are registered".into());
        }
        return Err(HubError::Unsatisfiable { reasons });
    }
    eligible.sort_by(|a, b| {
        load_of(a, running)
            .partial_cmp(&load_of(b, running))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.host_id.cmp(&b.host_id))
    });
    Ok(eligible)
}

fn load_of(host: &HostRecord, running: &[(String, i64)]) -> f64 {
    let run = running
        .iter()
        .find(|(id, _)| id == &host.host_id)
        .map(|(_, n)| *n)
        .unwrap_or(host.instance_count);
    let cap = host.max_instances.max(1) as f64;
    run as f64 / cap
}

fn consider(
    host: &HostRecord,
    _running: &[(String, i64)],
    placement: &Placement,
    spec: &PlaceSpec,
) -> Result<(), String> {
    match placement {
        Placement::Host { host_id } if host.host_id != *host_id => {
            return Err(format!("{}: not the requested host", host.host_id));
        }
        Placement::Host { host_id } if !host.online => {
            return Err(format!("{host_id}: host is offline"));
        }
        Placement::Labels { labels } => {
            if !host.online {
                return Err(format!("{}: host is offline", host.host_id));
            }
            for wanted in labels {
                if !host_has_label(host, wanted) {
                    return Err(format!("{}: missing label {wanted}", host.host_id));
                }
            }
        }
        Placement::Any => {
            if !host.online {
                return Err(format!("{}: host is offline", host.host_id));
            }
        }
        Placement::Host { .. } => {}
    }
    if spec.driver == "claude-pty" && !has_herdr(host) {
        return Err(format!(
            "{}: driver claude-pty requires herdr",
            host.host_id
        ));
    }
    if spec.delegation.as_deref() == Some("gateway") && !has_gateway_egress(host) {
        return Err(format!(
            "{}: gateway profile requires label egress:gateway",
            host.host_id
        ));
    }
    Ok(())
}

fn host_has_label(host: &HostRecord, wanted: &str) -> bool {
    let want = normalize_label(wanted);
    host.labels.iter().any(|have| normalize_label(have) == want)
}

fn normalize_label(raw: &str) -> String {
    raw.trim().replace(':', "=")
}

fn has_herdr(host: &HostRecord) -> bool {
    let Some(herdr) = host.herdr.as_ref() else {
        return false;
    };
    if herdr.is_null() {
        return false;
    }
    ["version", "socket", "path"].iter().any(|key| {
        herdr
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

fn has_gateway_egress(host: &HostRecord) -> bool {
    host_has_label(host, "egress=gateway") || host_has_label(host, "egress:gateway")
}

/// Fields for [`spawn_on_host`].
pub struct SpawnRequest {
    /// Agent kind.
    pub kind: String,
    /// Driver kind.
    pub driver: String,
    /// Optional workspace.
    pub workspace_id: Option<String>,
    /// UI title.
    pub title: Option<String>,
    /// Optional first prompt.
    pub prompt: Option<String>,
    /// Opaque spec JSON stored on the instance.
    pub spec: Value,
}

/// Create an instance on an already-chosen host (fleet and HTTP create).
pub async fn spawn_on_host(
    state: &AppState,
    host: &HostRecord,
    request: SpawnRequest,
) -> Result<(InstanceRecord, CommandRecord), HubError> {
    let instance = state
        .store
        .insert_instance(
            host.host_id.clone(),
            request.workspace_id,
            request.kind,
            request.driver,
            request.title,
            request.spec.clone(),
        )
        .await
        .map_err(crate::http::map_store)?;
    let payload = json!({
        "instanceId": instance.instance_id,
        "spec": request.spec,
        "initialInput": request.prompt.as_ref().map(|text| json!({ "type": "prompt", "text": text })),
    });
    let (command, _) = state
        .store
        .queue_command(
            None,
            Some(instance.instance_id.clone()),
            host.host_id.clone(),
            "instance.create".into(),
            payload,
            None,
        )
        .await?;
    let command = crate::http::forward_if_online(state, command, host.online).await?;
    Ok((instance, command))
}

/// Load hosts + running counts and select.
pub async fn pick_hosts(
    state: &AppState,
    placement: &Placement,
    spec: &PlaceSpec,
) -> Result<Vec<HostRecord>, HubError> {
    let hosts = state.store.list_hosts().await?;
    let mut running = Vec::new();
    for host in &hosts {
        let n = state.store.running_count(host.host_id.clone()).await?;
        running.push((host.host_id.clone(), n));
    }
    select_hosts(&hosts, &running, placement, spec)
}

/// `POST /v1/placement/resolve` — dry-run selection.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/placement/resolve", post(resolve_http))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveBody {
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    placement: Option<Value>,
    #[serde(default)]
    spec: Value,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    delegation: Option<String>,
}

async fn resolve_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResolveBody>,
) -> Result<Json<Value>, HubError> {
    require_origin(&headers, &state.config)?;
    require_device(&state.store, &headers).await?;
    let placement = Placement::from_value(body.placement.as_ref(), body.host_id.as_deref())?;
    let mut spec = PlaceSpec::from_json(&body.spec);
    if let Some(driver) = body.driver {
        spec.driver = driver;
    }
    if body.delegation.is_some() {
        spec.delegation = body.delegation;
    }
    let hosts = pick_hosts(&state, &placement, &spec).await?;
    Ok(Json(json!({
        "hostId": hosts.first().map(|h| h.host_id.clone()),
        "hosts": hosts,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::HostRecord;

    fn host(id: &str, online: bool, labels: &[&str], herdr: bool, max: i64) -> HostRecord {
        HostRecord {
            host_id: id.into(),
            label: id.into(),
            state: if online {
                "online".into()
            } else {
                "offline".into()
            },
            online,
            last_seen_at: None,
            node_version: None,
            cli: json!([]),
            capabilities: json!({}),
            instance_count: 0,
            transport: "outbound-wss".into(),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            herdr: herdr.then(|| json!({"version": "0.9.0", "socket": "/tmp/herdr.sock"})),
            resources: None,
            max_instances: max,
            hostname: None,
        }
    }

    #[test]
    fn labels_and_herdr_and_load() {
        let a = host("hst_a", true, &["region=sg", "egress=gateway"], true, 8);
        let b = host("hst_b", true, &["region=cn"], false, 2);
        let hosts = [a, b];
        let running = vec![("hst_a".into(), 4), ("hst_b".into(), 0)];
        let spec = PlaceSpec {
            driver: "claude-print".into(),
            delegation: None,
        };
        let picked = select_hosts(
            &hosts,
            &running,
            &Placement::Labels {
                labels: vec!["region=sg".into()],
            },
            &spec,
        )
        .unwrap();
        assert_eq!(picked[0].host_id, "hst_a");

        let pty = PlaceSpec {
            driver: "claude-pty".into(),
            delegation: None,
        };
        let err = select_hosts(
            &hosts,
            &running,
            &Placement::Host {
                host_id: "hst_b".into(),
            },
            &pty,
        )
        .unwrap_err();
        match err {
            HubError::Unsatisfiable { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("herdr")));
            }
            other => panic!("{other}"),
        }
    }
}
