//! Local and Hub-routed host diagnostics; no credential values leave the checks.

use anyhow::Result;
use clap::Args;
use remuda_node::{DoctorContext, DoctorReport, ProbeEnv, doctor_port, doctor_snapshot};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

use super::hub_client::{HubClient, HubOpts, ResolveInput, block_on, print_json, resolve_hub};
use crate::config::Config;

#[derive(Clone, Debug, Default, Args, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DoctorArgs {
    /// Inspect a registered host through its active Hub link.
    #[arg(long, conflicts_with = "local")]
    pub host: Option<String>,
    /// Inspect this machine (the default).
    #[arg(long)]
    pub local: bool,
    /// Emit the complete structured report.
    #[arg(long)]
    pub json: bool,
}

pub(crate) fn run(config: Config, hub: HubOpts, args: DoctorArgs) -> Result<i32> {
    block_on(async move {
        let mut input = ResolveInput::from_opts(&hub);
        input.env_data_dir = Some(config.data_dir.clone());
        input.fallback_hub = Some(config_hub(&config));
        let mut resolved = resolve_hub(&input);
        if resolved.token.is_none() && resolved.bootstrap_token.is_none() {
            resolved.bootstrap_token = config
                .hub
                .bootstrap_token
                .as_ref()
                .and_then(|reference| reference.resolve().ok())
                .map(|secret| secret.into_string());
        }
        let client = HubClient::new(resolved.url, resolved.token, resolved.bootstrap_token)?;
        let result = inspect(&config, &args, &client).await?;
        if args.json {
            print_json(&result)?;
        } else {
            human(&result);
        }
        Ok(result["exitCode"].as_i64().unwrap_or(1) as i32)
    })
}

fn config_hub(config: &Config) -> String {
    if let Some(url) = &config.node.hub_url {
        let http = url
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1);
        // Node links use /v1/node; doctor uses the HTTP API on the same authority.
        if let Ok(uri) = http.parse::<axum::http::Uri>()
            && let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority())
        {
            return format!("{scheme}://{authority}");
        }
    }
    let mut address = config.hub.listen;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    format!("http://{address}")
}

pub(crate) async fn inspect(
    config: &Config,
    args: &DoctorArgs,
    client: &HubClient,
) -> Result<Value> {
    anyhow::ensure!(
        !(args.host.is_some() && args.local),
        "host and local are mutually exclusive"
    );
    let hosts = tokio::time::timeout(Duration::from_secs(10), client.list_hosts()).await;
    let (registered, reachable) = match hosts {
        Ok(Ok(items)) => (items, true),
        _ => (Vec::new(), false),
    };
    let mut report = if let Some(id) = &args.host {
        let mut fallback = DoctorReport::empty();
        match registered.iter().find(|host| host["hostId"] == *id) {
            Some(host) if host["online"] == true => {
                // IDs are selected from the authenticated registry, and still
                // checked before interpolation into a URL path.
                anyhow::ensure!(
                    !id.is_empty()
                        && id
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                    "invalid host id"
                );
                match tokio::time::timeout(
                    Duration::from_secs(25),
                    client.get(&format!("/v1/hosts/{id}/doctor")),
                )
                .await
                {
                    Ok(Ok(value)) => match serde_json::from_value::<DoctorReport>(value) {
                        Ok(report) => report,
                        Err(_) => {
                            fallback.check(
                                "host.diagnostics",
                                "blocker",
                                "host returned an invalid diagnostics report",
                                Value::Null,
                            );
                            fallback
                        }
                    },
                    _ => {
                        fallback.check("host.diagnostics", "blocker", "fresh host diagnostics unavailable; upgrade Hub/Node or check its link", Value::Null);
                        fallback
                    }
                }
            }
            Some(_) => {
                fallback.check(
                    "host.link",
                    "blocker",
                    "selected host has no active link",
                    json!({"hostId":id}),
                );
                fallback
            }
            None => {
                fallback.check(
                    "host.registry",
                    "blocker",
                    "selected host was not found in the reachable registry",
                    json!({"hostId":id}),
                );
                fallback
            }
        }
    } else {
        let context = DoctorContext {
            data_dir: Some(config.data_dir.clone()),
            listeners: Vec::new(),
        };
        let mut report = tokio::task::spawn_blocking(move || {
            doctor_snapshot(&context, ProbeEnv::from_process())
        })
        .await?;
        let owned_listeners = local_node_listeners(&report, &registered, client).await;
        for (name, address) in [
            ("port.hub", config.hub.listen),
            ("port.node", config.node.listen),
        ] {
            // An authenticated Hub on this exact address is an expected owner.
            let authority = client
                .base()
                .split("://")
                .nth(1)
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("");
            let hub_address = authority.parse::<std::net::SocketAddr>().ok();
            if (reachable
                && name == "port.hub"
                && hub_address.is_some_and(|hub| same_listener(address, hub)))
                || (name == "port.node"
                    && owned_listeners
                        .iter()
                        .any(|owned| same_listener(address, *owned)))
            {
                report.check(
                    name,
                    "ok",
                    "configured port is owned by the authenticated Hub/Node",
                    json!({"address":address}),
                );
            } else {
                doctor_port(&mut report, name, address);
            }
        }
        if config.hub.listen == config.node.listen && config.hub.listen.port() != 0 {
            report.check(
                "ports.overlap",
                "blocker",
                "Hub and Node listeners use the same address",
                Value::Null,
            );
        }
        report
    };
    report.check(
        "hub.reachability",
        if reachable { "ok" } else { "blocker" },
        if reachable {
            "authenticated Hub registry is reachable"
        } else {
            "Hub unreachable or authentication failed; check --hub and credential configuration"
        },
        Value::Null,
    );
    let mut value = serde_json::to_value(report)?;
    value["mode"] = json!(if args.host.is_some() {
        "remote"
    } else {
        "local"
    });
    value["hostId"] = json!(args.host);
    value["registeredHosts"] = json!(registered.iter().map(|host| json!({
        "hostId":host["hostId"], "name":host["name"], "label":host["label"],
        "online":host["online"], "state":host["state"], "lastSeenAt":host["lastSeenAt"], "transport":host["transport"]
    })).collect::<Vec<_>>());
    Ok(value)
}

fn same_listener(configured: std::net::SocketAddr, actual: std::net::SocketAddr) -> bool {
    configured.port() != 0
        && configured.port() == actual.port()
        && (configured.ip() == actual.ip()
            || (configured.ip().is_unspecified() && actual.ip().is_loopback()))
}

async fn local_node_listeners(
    report: &DoctorReport,
    registered: &[Value],
    client: &HubClient,
) -> Vec<std::net::SocketAddr> {
    let id = report
        .checks
        .iter()
        .filter(|check| matches!(check.name.as_str(), "identity" | "identity.node"))
        .filter_map(|check| check.details["hostId"].as_str())
        .find(|id| {
            registered
                .iter()
                .any(|host| host["hostId"] == *id && host["online"] == true)
        });
    let Some(id) = id else {
        return Vec::new();
    };
    let Ok(Ok(value)) = tokio::time::timeout(
        Duration::from_secs(25),
        client.get(&format!("/v1/hosts/{id}/doctor")),
    )
    .await
    else {
        return Vec::new();
    };
    value["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == "listeners"))
        .and_then(|check| check["details"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().and_then(|value| value.parse().ok()))
        .collect()
}

fn human(value: &Value) {
    if let Some(checks) = value["checks"].as_array() {
        for check in checks {
            println!(
                "{:<8} {:<24} {} {}",
                text(&check["status"]),
                text(&check["name"]),
                text(&check["message"]),
                check["details"]
            );
        }
    }
    println!("Registered hosts:");
    if let Some(hosts) = value["registeredHosts"].as_array() {
        for host in hosts {
            println!(
                "  {} {} link={} ({})",
                text(&host["hostId"]),
                text(&host["label"]),
                if host["online"] == true {
                    "online"
                } else {
                    "offline"
                },
                text(&host["transport"])
            );
        }
    }
    println!(
        "doctor: {}",
        if value["exitCode"] == 0 {
            "no blockers"
        } else {
            "blocked"
        }
    );
}

fn text(value: &Value) -> String {
    super::table::cell(value.as_str().unwrap_or("-"), 160)
}
