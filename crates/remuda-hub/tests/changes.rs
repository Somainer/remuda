//! G2 workspace-changes proxy: operator scope and 403 / 409 / 422 mapping.
//!
//! The Node's own read-only computation is tested in `remuda-node`; here we
//! only assert the Hub boundary — operator-only, no cache, offline → 409,
//! unwritable session → 422 — against a scripted in-process Node.

use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};

const CHANGES: &str = "/v1/hosts/hst_changes001/workspaces/wsp_project/changes";

async fn get(client: &reqwest::Client, addr: std::net::SocketAddr, path: &str, token: &str) -> u16 {
    client
        .get(format!("http://{addr}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn changes_proxy_enforces_operator_and_maps_offline_states() {
    let dir = tempfile::tempdir().unwrap();
    let hub = spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .unwrap();
    let addr = hub.addr;
    let client = reqwest::Client::new();
    let human = hub.mint_device_token("changes-phone").await.unwrap();
    let agent = hub
        .test_mint_agent_token("changes-agent", "ins_changes001")
        .await
        .unwrap();

    // Online scripted Node with a valid status body.
    hub.test_insert_host("hst_changes001").await.unwrap();
    hub.test_set_node_reply(
        "hst_changes001",
        Some(json!({"result": {
            "workspaceId": "wsp_project",
            "availability": "ok",
            "entries": [],
            "observedAt": "2026-09-14T00:00:00.000Z",
        }})),
    )
    .await;

    // 200: operator gets the Node body untouched (no Hub-side rewrite).
    let response = client
        .get(format!("http://{addr}{CHANGES}"))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["availability"], "ok");
    assert_eq!(body["workspaceId"], "wsp_project");

    // diff and file proxies accept the workspace-relative path query.
    let diff = client
        .get(format!(
            "http://{addr}/v1/hosts/hst_changes001/workspaces/wsp_project/changes/diff?path=src/a.rs"
        ))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(diff.status(), 200);
    let file = client
        .get(format!(
            "http://{addr}/v1/hosts/hst_changes001/workspaces/wsp_project/changes/file?path=src/a.rs"
        ))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(file.status(), 200);

    // 403: the in-session agent credential is rejected on all three reads,
    // regardless of host state (fail-closed before any Node contact).
    assert_eq!(get(&client, addr, CHANGES, &agent).await, 403);
    assert_eq!(
        get(
            &client,
            addr,
            "/v1/hosts/hst_changes001/workspaces/wsp_project/changes/diff?path=src/a.rs",
            &agent,
        )
        .await,
        403
    );
    assert_eq!(
        get(
            &client,
            addr,
            "/v1/hosts/hst_changes001/workspaces/wsp_project/changes/file?path=src/a.rs",
            &agent,
        )
        .await,
        403
    );

    // 401: no credential at all.
    let anonymous = client
        .get(format!("http://{addr}{CHANGES}"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);

    // 404: host that was never registered.
    let unknown = get(
        &client,
        addr,
        "/v1/hosts/hst_missing000/workspaces/wsp_project/changes",
        &human,
    )
    .await;
    assert_eq!(unknown, 404);

    // 409: registered host with no live Node → HOST_OFFLINE, no stale content.
    hub.test_disconnect_node("hst_changes001").await;
    let offline = client
        .get(format!("http://{addr}{CHANGES}"))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(offline.status(), 409);
    let offline_body: Value = offline.json().await.unwrap();
    assert_eq!(offline_body["code"], "HOST_OFFLINE");

    // 422: a connected but unwritable Node session → PLACEMENT_UNSATISFIABLE.
    hub.test_set_node_reply("hst_changes001", None).await;
    let unsatisfiable = client
        .get(format!("http://{addr}{CHANGES}"))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(unsatisfiable.status(), 422);
    let body: Value = unsatisfiable.json().await.unwrap();
    assert_eq!(body["code"], "PLACEMENT_UNSATISFIABLE");
}

#[tokio::test]
async fn changes_proxy_returns_node_error_without_caching_it() {
    let dir = tempfile::tempdir().unwrap();
    let hub = spawn(HubConfig::for_test(dir.path().join("data")))
        .await
        .unwrap();
    let human = hub.mint_device_token("changes-phone2").await.unwrap();
    hub.test_insert_host("hst_changes002").await.unwrap();

    // The Node answers structured 4xx for an unregistered workspace. The Hub
    // forwards it as 400 and keeps nothing cached: a later successful answer is
    // returned verbatim.
    hub.test_set_node_reply(
        "hst_changes002",
        Some(json!({"error": {"code": -32000, "message": "workspace not found: wsp_gone"}})),
    )
    .await;
    let missing = reqwest::Client::new()
        .get(format!(
            "http://{}/v1/hosts/hst_changes002/workspaces/wsp_gone/changes",
            hub.addr
        ))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
    assert!(missing.text().await.unwrap().contains("wsp_gone"));

    hub.test_set_node_reply(
        "hst_changes002",
        Some(json!({"result": {"availability": "ok", "entries": [{"xy": " M"}]}})),
    )
    .await;
    let recovered: Value = reqwest::Client::new()
        .get(format!(
            "http://{}/v1/hosts/hst_changes002/workspaces/wsp_gone/changes",
            hub.addr
        ))
        .bearer_auth(&human)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recovered["entries"][0]["xy"], " M");
}
