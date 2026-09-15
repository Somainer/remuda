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
    /// A project's member hosts (design §3.4 step 1); per-host project
    /// quotas and `requires` labels ride in [`PlaceConstraints`].
    Project {
        /// `prj_…`.
        project_id: String,
    },
}

/// Resource-utilization ceiling read at solve time; design §3.4 step 2.
///
/// `HostRecord.resources` (`{cpuPct, memPct}`, collected by remuda-node
/// inventory) has been stored for ages but never read by host selection.
/// A snapshot at/above the line removes the host with a machine-readable
/// reason; absent snapshots do not exclude (older Nodes simply report none).
pub const RESOURCE_CPU_PCT_MAX: u8 = 90;
/// See [`RESOURCE_CPU_PCT_MAX`].
pub const RESOURCE_MEM_PCT_MAX: u8 = 90;

/// Read `(cpuPct, memPct)` off a host's last inventory snapshot.
pub(crate) fn resource_pressure(host: &HostRecord) -> (Option<u8>, Option<u8>) {
    let resources = host.resources.as_ref();
    let cpu = resources
        .and_then(|value| value.get("cpuPct"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u8::try_from(n).ok());
    let mem = host
        .resources
        .as_ref()
        .and_then(|value| value.get("memPct"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u8::try_from(n).ok());
    (cpu, mem)
}

/// Project layer resolved before solving; design §3.4.
#[derive(Clone, Debug, Default)]
pub struct PlaceConstraints {
    /// When set, only these hosts are eligible (project member hosts).
    pub member_hosts: Option<std::collections::HashSet<String>>,
    /// Per-host project quotas, keyed by `hst_…`.
    pub project_hosts: std::collections::HashMap<String, ProjectHostPlace>,
}

/// One project-side host entry as the solver sees it.
#[derive(Clone, Debug, Default)]
pub struct ProjectHostPlace {
    /// Project cap, clamped to the host's own `maxInstances`.
    pub max_instances: Option<i64>,
    /// Hard label requirements normalized to `key=value`.
    pub requires: Vec<String>,
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
        // Project placement wins over a sibling `labels` array: project
        // labels/requires arrive through PlaceConstraints instead.
        if let Some(project_id) = value
            .get("projectId")
            .or_else(|| value.get("project"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Ok(Self::Project {
                project_id: project_id.to_string(),
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
            Some("project") => {
                let project_id = value
                    .get("projectId")
                    .or_else(|| value.get("project"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        HubError::BadRequest("placement.project requires projectId".into())
                    })?;
                Ok(Self::Project {
                    project_id: project_id.to_string(),
                })
            }
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
    constraints: &PlaceConstraints,
) -> Result<Vec<HostRecord>, HubError> {
    let mut reasons = Vec::new();
    let mut eligible = Vec::new();
    for host in hosts {
        match consider(host, running, placement, spec, constraints) {
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
    running: &[(String, i64)],
    placement: &Placement,
    spec: &PlaceSpec,
    constraints: &PlaceConstraints,
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
        Placement::Project { project_id } => {
            if !host.online {
                return Err(format!("{}: host is offline", host.host_id));
            }
            if !constraints
                .member_hosts
                .as_ref()
                .is_none_or(|members| members.contains(&host.host_id))
            {
                return Err(format!(
                    "{}: not a member host of project {project_id}",
                    host.host_id
                ));
            }
            if let Some(project_host) = constraints.project_hosts.get(&host.host_id) {
                for wanted in &project_host.requires {
                    if !host_has_label(host, wanted) {
                        return Err(format!(
                            "{}: project {project_id} requires label {wanted}",
                            host.host_id
                        ));
                    }
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
    let running_n = running
        .iter()
        .find(|(id, _)| id == &host.host_id)
        .map(|(_, n)| *n)
        .unwrap_or(host.instance_count);
    // A project quota clamps, never raises, the host's own ceiling.
    let mut max_instances = host.max_instances;
    if let Some(project_host) = constraints.project_hosts.get(&host.host_id)
        && let Some(project_cap) = project_host.max_instances
    {
        max_instances = max_instances.min(project_cap);
    }
    if running_n >= max_instances {
        // `running_n` counts only Node-confirmed live instances plus creates
        // still inside their acknowledgement window, so this is a real
        // ceiling, not a pile of stale `requested` rows (see
        // `store::LIVE_INSTANCE_COUNT_SQL`). Raise it with
        // `PATCH /v1/hosts/{id} {"maxInstances":N}`, which now persists across
        // Node hello and Hub restarts.
        return Err(format!(
            "{}: at maxInstances {max_instances} ({running_n} live); raise it with PATCH /v1/hosts/{}",
            host.host_id, host.host_id
        ));
    }
    // Design §3.4 step 2: the inventory snapshot the Node already reports is
    // finally read here. A missing snapshot never excludes (older Nodes, first
    // hello), matching the best-effort nature of the field.
    let (cpu_pct, mem_pct) = resource_pressure(host);
    if let Some(cpu) = cpu_pct
        && cpu >= RESOURCE_CPU_PCT_MAX
    {
        return Err(format!(
            "{}: host CPU at {cpu}% (limit {RESOURCE_CPU_PCT_MAX}%)",
            host.host_id
        ));
    }
    if let Some(mem) = mem_pct
        && mem >= RESOURCE_MEM_PCT_MAX
    {
        return Err(format!(
            "{}: host memory at {mem}% (limit {RESOURCE_MEM_PCT_MAX}%)",
            host.host_id
        ));
    }
    if host.ssh.is_some() && spec.driver == "generic-pty" && !has_herdr(host) {
        return Err(format!(
            "{}: remote generic-pty requires herdr; no shell-pty driver is advertised by this Node. Use a host with herdr or a supported no-herdr driver",
            host.host_id
        ));
    }
    if spec.driver == "claude-pty" && !has_herdr(host) {
        return Err(format!(
            "{}: driver claude-pty requires herdr",
            host.host_id
        ));
    }
    Ok(())
}

pub(crate) fn host_has_label(host: &HostRecord, wanted: &str) -> bool {
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
    if host.ssh.is_some() {
        return herdr
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.is_empty());
    }
    ["version", "socket", "path"].iter().any(|key| {
        herdr
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
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
    /// Node RPC to queue (`instance.create`, or `instance.resume` for D-026).
    pub operation: &'static str,
    /// Idempotency key so a retried spawn reuses the queued command.
    pub idempotency_key: Option<String>,
    /// Delegation-tree state (scope, grants, preset, task); §2.5.
    pub delegation: crate::store::InstanceDelegation,
}

/// Hosts with `online` derived from a live Hub<->Node session, not SQLite state.
pub async fn hosts_with_live_links(state: &AppState) -> Result<Vec<HostRecord>, HubError> {
    let live = state.nodes.host_ids().await;
    Ok(state
        .store
        .list_hosts()
        .await?
        .into_iter()
        .map(|host| {
            let connected = live.iter().any(|id| id == &host.host_id);
            crate::store::Store::with_live_link(host, connected)
        })
        .collect())
}

/// Create an instance on an already-chosen host (fleet and HTTP create).
pub async fn spawn_on_host(
    state: &AppState,
    host: &HostRecord,
    request: SpawnRequest,
) -> Result<(InstanceRecord, CommandRecord), HubError> {
    if state.nodes.kind_of(&host.host_id).await.is_none() {
        return Err(HubError::HostOffline {
            host_id: host.host_id.clone(),
        });
    }
    let instance = state
        .store
        .insert_instance_delegated(
            host.host_id.clone(),
            request.workspace_id,
            request.kind,
            request.driver,
            request.title,
            request.spec.clone(),
            request.delegation,
        )
        .await
        .map_err(crate::http::map_store)?;
    let payload = json!({
        "origin": request.spec.get("origin").cloned().unwrap_or(json!("agent")),
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
            request.operation.to_owned(),
            payload,
            request.idempotency_key,
        )
        .await?;
    let command = crate::http::forward_if_online(state, command, true).await?;
    Ok((instance, command))
}

/// Load hosts + running counts and select.
pub async fn pick_hosts(
    state: &AppState,
    placement: &Placement,
    spec: &PlaceSpec,
) -> Result<Vec<HostRecord>, HubError> {
    if let Placement::Host { host_id } = placement {
        let exists = state.store.get_host(host_id.clone()).await?.is_some();
        if exists && state.nodes.kind_of(host_id).await.is_none() {
            return Err(HubError::HostOffline {
                host_id: host_id.clone(),
            });
        }
    }
    let constraints = project_constraints(state, placement).await?;
    let hosts = hosts_with_live_links(state).await?;
    let mut running = Vec::new();
    for host in &hosts {
        let n = state.store.running_count(host.host_id.clone()).await?;
        running.push((host.host_id.clone(), n));
    }
    select_hosts(&hosts, &running, placement, spec, &constraints)
}

/// Resolve a [`Placement::Project`] into member hosts + per-host quotas;
/// design §3.4 step 1.
pub async fn project_constraints(
    state: &AppState,
    placement: &Placement,
) -> Result<PlaceConstraints, HubError> {
    let Placement::Project { project_id } = placement else {
        return Ok(PlaceConstraints::default());
    };
    let project = state
        .store
        .get_project(project_id.clone())
        .await?
        .ok_or(HubError::NotFound)?;
    let member_hosts = project
        .members
        .iter()
        .map(|member| member.host_id.as_id().to_string())
        .collect::<std::collections::HashSet<_>>();
    let project_hosts = project
        .hosts
        .iter()
        .map(|quota| {
            (
                quota.host_id.as_id().to_string(),
                ProjectHostPlace {
                    max_instances: quota.max_instances,
                    requires: quota.requires.clone(),
                },
            )
        })
        .collect();
    Ok(PlaceConstraints {
        member_hosts: Some(member_hosts),
        project_hosts,
    })
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
        "projectId": match &placement {
            Placement::Project { project_id } => Some(project_id.clone()),
            _ => None,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::HostRecord;

    fn host(id: &str, online: bool, labels: &[&str], herdr: bool, max: i64) -> HostRecord {
        HostRecord {
            workspaces: Vec::new(),
            workspace_revision: 0,
            ssh: None,
            last_error: None,
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
            provider_binding: "auto".into(),
            default_launch_args: None,
            claude_binary_path: None,
            default_tui: None,
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
            &PlaceConstraints::default(),
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
            &PlaceConstraints::default(),
        )
        .unwrap_err();
        match err {
            HubError::Unsatisfiable { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("herdr")));
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn at_capacity_host_is_unsatisfiable() {
        let a = host("hst_full", true, &[], false, 2);
        let hosts = [a];
        let running = vec![("hst_full".into(), 2)];
        let spec = PlaceSpec {
            driver: "claude-print".into(),
            delegation: None,
        };
        let err = select_hosts(
            &hosts,
            &running,
            &Placement::Any,
            &spec,
            &PlaceConstraints::default(),
        )
        .unwrap_err();
        match err {
            HubError::Unsatisfiable { reasons } => {
                assert!(
                    reasons.iter().any(|r| r.contains("maxInstances")),
                    "{reasons:?}"
                );
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn managed_ssh_requires_ready_link_and_real_herdr_executable() {
        let mut remote = host("hst_remote", true, &[], true, 8);
        remote.ssh = Some(json!({"target": "test-node"}));
        remote.state = "connecting".into();
        assert!(!crate::store::Store::with_live_link(remote.clone(), true).online);
        remote.state = "retired".into();
        assert!(!crate::store::Store::with_live_link(remote.clone(), true).online);
        remote.state = "online".into();
        assert!(!crate::store::Store::with_live_link(remote.clone(), false).online);
        assert!(crate::store::Store::with_live_link(remote.clone(), true).online);
        let spec = PlaceSpec {
            driver: "generic-pty".into(),
            delegation: None,
        };
        let error = consider(
            &remote,
            &[],
            &Placement::Any,
            &spec,
            &PlaceConstraints::default(),
        )
        .unwrap_err();
        assert!(error.contains("requires herdr") && error.contains("shell-pty"));
        remote.herdr = Some(json!({"path": "/usr/bin/herdr", "version": "0.9.0"}));
        assert!(
            consider(
                &remote,
                &[],
                &Placement::Any,
                &spec,
                &PlaceConstraints::default()
            )
            .is_ok()
        );
        remote.herdr = None;
        assert!(
            consider(
                &remote,
                &[],
                &Placement::Any,
                &PlaceSpec {
                    driver: "claude-print".into(),
                    delegation: None
                },
                &PlaceConstraints::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn project_placement_restricts_to_member_hosts_and_requires() {
        let member = host("hst_sg", true, &["toolchain=rust"], false, 8);
        let outsider = host("hst_other", true, &["toolchain=rust"], false, 8);
        let unlabeled = host("hst_mac", true, &[], false, 8);
        let constraints = PlaceConstraints {
            member_hosts: Some(
                ["hst_sg".to_string(), "hst_mac".to_string()]
                    .into_iter()
                    .collect(),
            ),
            project_hosts: [
                (
                    "hst_sg".to_string(),
                    ProjectHostPlace {
                        max_instances: Some(8),
                        requires: vec!["toolchain=rust".into()],
                    },
                ),
                (
                    "hst_mac".to_string(),
                    ProjectHostPlace {
                        max_instances: Some(8),
                        requires: vec!["toolchain=rust".into()],
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let spec = PlaceSpec {
            driver: "claude-print".into(),
            delegation: None,
        };
        let err = select_hosts(
            &[outsider, unlabeled],
            &[],
            &Placement::Project {
                project_id: "prj_test".into(),
            },
            &spec,
            &constraints,
        )
        .unwrap_err();
        match err {
            HubError::Unsatisfiable { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| r.contains("not a member host") && r.contains("hst_other")),
                    "{reasons:?}"
                );
                assert!(
                    reasons
                        .iter()
                        .any(|r| r.contains("requires label") && r.contains("hst_mac")),
                    "{reasons:?}"
                );
            }
            other => panic!("{other}"),
        }
        let picked = select_hosts(
            &[member],
            &[],
            &Placement::Project {
                project_id: "prj_test".into(),
            },
            &spec,
            &constraints,
        )
        .unwrap();
        assert_eq!(picked[0].host_id, "hst_sg");
    }

    #[test]
    fn project_quota_clamps_host_capacity() {
        let mut record = host("hst_sg", true, &[], false, 8);
        record.instance_count = 4;
        let constraints = PlaceConstraints {
            member_hosts: Some(["hst_sg".to_string()].into_iter().collect()),
            project_hosts: [(
                "hst_sg".to_string(),
                ProjectHostPlace {
                    max_instances: Some(4),
                    requires: vec![],
                },
            )]
            .into_iter()
            .collect(),
        };
        let spec = PlaceSpec {
            driver: "claude-print".into(),
            delegation: None,
        };
        let err = consider(
            &record,
            &[],
            &Placement::Project {
                project_id: "prj_test".into(),
            },
            &spec,
            &constraints,
        )
        .unwrap_err();
        assert!(err.contains("maxInstances 4"), "{err}");
    }

    #[test]
    fn saturated_cpu_or_mem_resources_exclude_a_host() {
        let mut busy = host("hst_busy", true, &[], false, 8);
        busy.resources = Some(json!({ "cpuPct": 91, "memPct": 40 }));
        let spec = PlaceSpec {
            driver: "claude-print".into(),
            delegation: None,
        };
        assert_eq!(resource_pressure(&busy), (Some(91), Some(40)));
        let err = consider(
            &busy,
            &[],
            &Placement::Any,
            &spec,
            &PlaceConstraints::default(),
        )
        .unwrap_err();
        assert!(err.contains("CPU at 91%"), "{err}");
        busy.resources = Some(json!({ "cpuPct": 5, "memPct": 95 }));
        let err = consider(
            &busy,
            &[],
            &Placement::Any,
            &spec,
            &PlaceConstraints::default(),
        )
        .unwrap_err();
        assert!(err.contains("memory at 95%"), "{err}");
        // Below the line, and absent snapshots, the host stays eligible.
        busy.resources = Some(json!({ "cpuPct": 89, "memPct": 89 }));
        assert!(
            consider(
                &busy,
                &[],
                &Placement::Any,
                &spec,
                &PlaceConstraints::default()
            )
            .is_ok()
        );
        busy.resources = None;
        assert!(
            consider(
                &busy,
                &[],
                &Placement::Any,
                &spec,
                &PlaceConstraints::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn project_placement_parses() {
        assert_eq!(
            Placement::from_value(Some(&json!({"kind":"project","projectId":"prj_x"})), None)
                .unwrap(),
            Placement::Project {
                project_id: "prj_x".into()
            }
        );
        assert_eq!(
            Placement::from_value(Some(&json!({"projectId":"prj_y"})), None).unwrap(),
            Placement::Project {
                project_id: "prj_y".into()
            }
        );
        let err = Placement::from_value(Some(&json!({"kind":"project"})), None).unwrap_err();
        assert!(err.to_string().contains("projectId"), "{err}");
    }
}
