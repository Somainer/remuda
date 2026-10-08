//! c-dirpicker round 6 item 4 / round 7 items 4-5: unregister against the
//! PRODUCTION Node runtime.
//!
//! Unlike the scripted-node route tests, every frame here is handled by real
//! code: the in-process Hub (`remuda_hub::spawn`), a real outbound-WSS
//! runtime (`WssLink::connect_runtime`) with the NATIVE shell-pty driver,
//! real admission (`POST /v1/instances` → a real login shell in a PTY), the
//! Node's own occupancy count at unregister prepare, and its real
//! `workspace.resolve` identity RPC.
//!
//! Sequence:
//! 1. create a session through real admission and positively await the
//!    NODE-side Ready state (the process exists), not just the Hub's
//!    pre-spawn `preparing` row (r7 item 4);
//! 2. mark ONLY the Hub row `failed` — the Node still runs the shell;
//! 3. DELETE must surface the production Node refusal verbatim (409,
//!    `state conflict:` / `(session history is kept)`, never the Hub's own
//!    "end or archive them" guard text) and leave the workspace registered;
//! 4. close the session for real and wait for the process to end;
//! 5. DELETE again must settle 200 and remove the workspace from the
//!    snapshot. A 409 after exit is NOT success.
//!
//! Every spawned process is reclaimed unconditionally: `Context` owns an
//! implicit Drop guard that runs on EVERY exit path (setup `?` failure,
//! panic, normal return). Its teardown HTTP requests are fallible and
//! bounded (no expect/panic before node.shutdown), and the native
//! driver-reclaim path (`node.shutdown`, which kills the PTY process group)
//! always runs before the hub stops, so no shell outlives the test binary.

use std::sync::Mutex;

use remuda_hub::HubConfig;
use remuda_node::{
    Backoff, DevServerConfig, LocalDrivers, NativeDriverConfig, ServeConfig, WssConfig, WssLink,
    compose,
};
use remuda_protocol::HostId;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Bounded, infallible single HTTP exchange for cleanup: unlike the
/// assertion-based `http` helper it never panics or blocks teardown when the
/// hub is already gone (r7 item 5).
async fn http_best_effort(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&str>,
) -> Option<(u16, String)> {
    let fut = async {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        let content_length = body.map(str::len).unwrap_or(0);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {content_length}\r\n"
        );
        if let Some(cookie) = cookie {
            head.push_str(&format!("Cookie: {cookie}\r\n"));
        }
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.ok()?;
        if let Some(body) = body {
            stream.write_all(body.as_bytes()).await.ok()?;
        }
        let mut buf = Vec::new();
        AsyncReadExt::read_to_end(&mut stream, &mut buf)
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&buf);
        let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Some((status, rest.to_string()))
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .ok()
        .flatten()
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    let content_length = body.map(str::len).unwrap_or(0);
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {content_length}\r\n"
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).await.expect("write");
    }
    let mut buf = Vec::new();
    AsyncReadExt::read_to_end(&mut stream, &mut buf)
        .await
        .expect("read");
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (status, rest.to_string())
}

fn cookie_from(head: &str) -> Option<String> {
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> String {
    let body = json!({ "bootstrapToken": bootstrap, "deviceName": "dirpicker-r6" }).to_string();
    let mut stream = TcpStream::connect(addr).await.expect("tcp");
    stream
        .write_all(
            format!(
                "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    let mut buf = Vec::new();
    AsyncReadExt::read_to_end(&mut stream, &mut buf)
        .await
        .expect("read");
    let text = String::from_utf8_lossy(&buf);
    let head = text.split_once("\r\n\r\n").map(|(h, _)| h).unwrap_or(&text);
    assert!(head.contains(" 200 "), "{head}");
    cookie_from(head).expect("set-cookie")
}

async fn poll_json(
    addr: std::net::SocketAddr,
    cookie: &str,
    path: &str,
    mut ready: impl FnMut(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (status, body) = http(addr, "GET", path, Some(cookie), None).await;
        assert_eq!(status, 200, "{path}: {body}");
        let value: Value = serde_json::from_str(body.trim()).expect("json");
        if ready(&value) {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting on {path}: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct Handles {
    hub: remuda_hub::RunningHub,
    link: WssLink,
    node: remuda_node::DevNode,
}

/// All owned handles, so the spawned login shell is reclaimed on EVERY exit
/// path (assertion panic, setup `?` failure and normal return): `Drop`
/// performs the same bounded teardown as the explicit `cleanup`. The close
/// HTTP request is fallible/bounded, the WSS link and native runtime are shut
/// with timeouts, and the native reclaim (`node.shutdown` — kills the PTY
/// child process group) ALWAYS runs before the hub stops. The temp tree
/// outlives the runtime and is deleted last.
struct Context {
    addr: std::net::SocketAddr,
    cookie: String,
    handles: Option<Handles>,
    instance: Mutex<Option<String>>,
    handle: tokio::runtime::Handle,
    /// Kept until cleanup so the real temp tree outlives the runtime.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

/// Shared teardown body. `handles` is taken exactly once; the instance close
/// is best-effort and bounded and runs only when the hub is still alive.
async fn run_teardown(
    handles: Handles,
    addr: std::net::SocketAddr,
    cookie: String,
    instance: Option<String>,
) {
    if let Some(instance) = instance {
        let _ = http_best_effort(
            addr,
            "POST",
            &format!("/v1/instances/{instance}/commands"),
            Some(&cookie),
            Some(&json!({"operation":"instance.close","payload":{}}).to_string()),
        )
        .await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), handles.link.shutdown()).await;
    // NATIVE reclamation, unconditionally: abort workers and close every
    // retained driver (kills the login shell's PTY group).
    if let Err(error) = tokio::time::timeout(Duration::from_secs(8), handles.node.shutdown()).await
    {
        eprintln!("node shutdown bounded after {error:?}");
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), handles.hub.shutdown()).await;
}

impl Context {
    fn set_instance(&self, id: String) {
        *self.instance.lock().unwrap() = Some(id);
    }

    async fn cleanup(mut self) {
        if let Some(handles) = self.handles.take() {
            let instance = self.instance.lock().ok().and_then(|mut slot| slot.take());
            run_teardown(handles, self.addr, self.cookie.clone(), instance).await;
        }
        // Dropping now only removes the temp dir.
        drop(self);
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // An early return/panic skipped the explicit cleanup: run the FULL
        // bounded teardown from a real OS thread, because block_on from one
        // of the multi_thread runtime's worker threads is rejected. This is
        // what guarantees the native login shell never outlives the binary.
        let Some(handles) = self.handles.take() else {
            return;
        };
        let instance = self.instance.lock().ok().and_then(|mut slot| slot.take());
        let addr = self.addr;
        let cookie = self.cookie.clone();
        let handle = self.handle.clone();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                handle.block_on(run_teardown(handles, addr, cookie, instance));
            });
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_refused_by_the_live_native_node_then_settles_after_exit() {
    let context = setup().await.expect("setup");
    let outcome = std::panic::AssertUnwindSafe(drive(
        context.addr,
        context.cookie.clone(),
        &context.handles.as_ref().expect("handles").hub,
        &context.handles.as_ref().expect("handles").node,
        &context,
    ))
    .catch_unwind()
    .await;
    context.cleanup().await;
    outcome.expect("real-node unregister flow");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_alias_shapes_against_the_real_runtime_fs() {
    // Round 6 item 5: symlink, symlink+`..`, and a real trailing-space
    // directory, resolved through the PRODUCTION Node's `workspace.resolve`
    // (reached over the real Hub DELETE route with an active task bound) — no
    // fabricated identity.
    let context = setup().await.expect("setup");
    let outcome = std::panic::AssertUnwindSafe(drive_aliases(
        context.addr,
        context.cookie.clone(),
        &context.handles.as_ref().expect("handles").hub,
        context.dir.path(),
    ))
    .catch_unwind()
    .await;
    context.cleanup().await;
    outcome.expect("real-node alias resolution");
}

use futures::FutureExt;

async fn setup() -> anyhow::Result<Context> {
    let dir = tempfile::tempdir()?;

    let hub = remuda_hub::spawn(HubConfig::for_test(dir.path().join("hub-data"))).await?;
    let addr = hub.addr;
    let cookie = login(addr, &hub.bootstrap_token).await;

    // Build the owning context BEFORE any later fallible step so a setup `?`
    // failure drops it and reclaims what already exists (r7 item 5). Handles
    // are filled in as they come up.
    let mut context = Context {
        addr,
        cookie,
        handles: None,
        instance: Mutex::new(None),
        handle: tokio::runtime::Handle::current(),
        dir,
    };

    // PRODUCTION Node runtime: native shell-pty driver; the temp workspace is
    // the startup root and dir.path is the existing allowlist root.
    let workspace = context.dir.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let node_data = context.dir.path().join("node-data");
    std::fs::create_dir_all(&node_data)?;
    let http_config = DevServerConfig::loopback(0)
        .with_workspace_root(workspace)
        .with_workspace_roots(vec![context.dir.path().to_path_buf()]);
    let node = compose(&ServeConfig {
        http: http_config,
        data_dir: node_data,
        drivers: LocalDrivers::Native(NativeDriverConfig::new(context.dir.path().to_path_buf())),
    })?;
    let host_id = HostId::new();
    let mut config = WssConfig::loopback(
        addr,
        hub.mint_enroll_token(remuda_hub::DEFAULT_ENROLL_TOKEN_TTL_MINUTES)
            .await?,
        host_id.as_id().as_str().to_owned(),
    );
    config.heartbeat_interval = Duration::from_secs(5);
    config.backoff = Backoff {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(100),
        jitter_ppt: 0,
    };
    let link =
        tokio::time::timeout(TIMEOUT, WssLink::connect_runtime(config, node.clone())).await??;
    context.handles = Some(Handles { hub, link, node });
    Ok(context)
}

async fn drive(
    addr: std::net::SocketAddr,
    cookie: String,
    hub: &remuda_hub::RunningHub,
    node: &remuda_node::DevNode,
    context: &Context,
) {
    // The runtime announces its host id itself; discover it from the fleet
    // view rather than reusing the pre-connect HostId (both are the same
    // value, but this exercises the production projection).
    let hosts = poll_json(addr, &cookie, "/v1/hosts", |value| {
        value["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|row| row["online"].as_bool() == Some(true))
        })
    })
    .await;
    let host_id = hosts["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["online"].as_bool() == Some(true))
        .and_then(|row| row["hostId"].as_str().or_else(|| row["id"].as_str()))
        .expect("online host id")
        .to_owned();

    // Wait for the REAL runtime to announce its registered workspace.
    let view = poll_json(
        addr,
        &cookie,
        &format!("/v1/hosts/{host_id}/workspaces"),
        |value| {
            value["workspaces"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        },
    )
    .await;
    let workspace = view["workspaces"][0].clone();
    let workspace_id = workspace["workspaceId"]
        .as_str()
        .expect("workspaceId")
        .to_owned();
    // The root used for every DELETE is read ONCE from the Node's own
    // snapshot (item 6's rule on the Rust side too).
    let root = workspace["root"].as_str().expect("root").to_owned();
    assert!(std::path::Path::new(&root).is_dir(), "{root}");

    // (1) Real admission: the native driver spawns a real login shell.
    let create = json!({
        "hostId": host_id,
        "workspaceId": workspace_id,
        "kind": "terminal",
        "driver": "shell-pty",
        "prompt": "c-dirpicker round 6 real-node unregister",
    })
    .to_string();
    let (status, body) = http(addr, "POST", "/v1/instances", Some(&cookie), Some(&create)).await;
    assert_eq!(status, 200, "real instance.create: {body}");
    let created: Value = serde_json::from_str(body.trim()).unwrap();
    let instance_id = created["instance"]["instanceId"]
        .as_str()
        .or_else(|| created["instance"]["id"].as_str())
        .expect("instanceId")
        .to_owned();
    context.set_instance(instance_id.clone());

    // r7 item 4: wait for POSITIVE Node-side evidence the process exists —
    // the production runtime only journals Ready AFTER driver.start() spawned
    // the PTY (runtime.rs publishes `preparing` BEFORE the worker exists, so
    // accepting that would let the test flip the Hub row before any process
    // was running). The NODE's own instance list is the liveness authority;
    // terminal shell-pty session reports Ready on the NODE while the Hub
    // projection can additionally surface `running`; Ready is the positive
    // post-spawn evidence (never preparing/starting/requested).
    let local_id: remuda_protocol::InstanceId = instance_id.parse().expect("instance id");
    let node_ready = || {
        node.list_instances().map(|page| {
            page.items
                .iter()
                .find(|instance| instance.meta.id == local_id)
                .is_some_and(|instance| {
                    matches!(
                        instance.lifecycle,
                        remuda_protocol::InstanceLifecycle::Ready
                    )
                })
        })
    };
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if node_ready().unwrap_or(false) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Node never reported a post-spawn live state for {instance_id}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The Hub projection must have observed the spawned session too. A
    // terminal session normally shows `running`; exclude every pre-spawn
    // state INCLUDING `preparing` (which the old predicate accepted even
    // though it is published before the worker/process exists).
    poll_json(
        addr,
        &cookie,
        &format!("/v1/instances/{instance_id}"),
        |value| {
            value["lifecycle"]
                .as_str()
                .is_some_and(|state| matches!(state, "ready" | "running"))
        },
    )
    .await;

    // (2) Mark ONLY the Hub row failed: a stale projection the Node knows
    //     nothing about. The Node keeps running the real shell.
    hub.test_mark_instance_failed(&instance_id)
        .await
        .expect("mark hub row failed");
    let (status, row) = http(
        addr,
        "GET",
        &format!("/v1/instances/{instance_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(
        serde_json::from_str::<Value>(&row).unwrap()["lifecycle"],
        "failed"
    );

    // (3) The Hub row says ended, so its own guard passes; the PRODUCTION
    //     Node prepare re-counts its live instance and refuses. The text is
    //     the Node's, surfaced as 409, and the workspace stays registered.
    let (status, body) = http(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host_id}/workspaces"),
        Some(&cookie),
        Some(&json!({"path": root}).to_string()),
    )
    .await;
    assert_eq!(
        status, 409,
        "production Node must refuse unregister while the shell is alive: {body}"
    );
    // r7 item 4: this MUST be the NODE's refusal (its local occupancy count),
    // proven by the NodeError::Conflict prefix and the Node-only suffix — and
    // it must NOT be the Hub's own occupancy guard (whose wording ends with
    // "end or archive them"). The Hub row was stale-failed, so its guard
    // passed; only the live Node could refuse.
    assert!(
        body.contains("state conflict:"),
        "refusal must be the Node's state conflict, not a generic channel error: {body}"
    );
    assert!(
        body.contains("(session history is kept)"),
        "Node refusal carries its history-kept suffix: {body}"
    );
    assert!(
        body.contains("live session(s)"),
        "production refusal text: {body}"
    );
    assert!(
        !body.contains("end or archive them"),
        "the Hub's own guard must not have fired (its row was failed); \
         this 409 must be the Node's live count: {body}"
    );
    let view = poll_json(
        addr,
        &cookie,
        &format!("/v1/hosts/{host_id}/workspaces"),
        |_| true,
    )
    .await;
    assert!(
        view["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["workspaceId"].as_str() == Some(workspace_id.as_str())),
        "refused unregister must leave the workspace registered: {view}"
    );

    // (4) Kill the real session process through the production close path.
    //     The Hub row already says `failed` (stale mark above), so it cannot
    //     prove process end: wait on the NODE's own live set instead — the
    //     Node is the liveness authority for exactly this race.
    let (status, closed) = http(
        addr,
        "POST",
        &format!("/v1/instances/{instance_id}/commands"),
        Some(&cookie),
        Some(&json!({"operation":"instance.close","payload":{}}).to_string()),
    )
    .await;
    assert_eq!(status, 200, "instance.close: {closed}");
    let local_id: remuda_protocol::InstanceId = instance_id.parse().expect("instance id");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        // Absent from the Node's instance list counts as ended: unregister's
        // own occupancy predicate filters this same list, so a missing row
        // occupies nothing. A query error keeps waiting until the deadline.
        let ended = node
            .list_instances()
            .map(|page| {
                page.items
                    .iter()
                    .find(|instance| instance.meta.id == local_id)
                    .is_none_or(|instance| {
                        matches!(
                            instance.lifecycle,
                            remuda_protocol::InstanceLifecycle::Exited
                                | remuda_protocol::InstanceLifecycle::Failed
                        )
                    })
            })
            .unwrap_or(false);
        if ended {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Node kept a live instance {instance_id} after close"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // (5) Second DELETE settles for real: 200 and the workspace is GONE from
    //     the snapshot. A 409 here would mean the process survived the close.
    let (status, body) = http(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host_id}/workspaces"),
        Some(&cookie),
        Some(&json!({"path": root}).to_string()),
    )
    .await;
    assert_eq!(
        status, 200,
        "after the process exits the unregister MUST settle (409 is not success): {body}"
    );
    let settled: Value = serde_json::from_str(body.trim()).unwrap();
    let gone = settled["workspaces"].as_array().is_none_or(|rows| {
        rows.iter()
            .all(|row| row["workspaceId"].as_str() != Some(workspace_id.as_str()))
    });
    assert!(
        gone,
        "the workspace must be gone from the settled snapshot: {settled}"
    );
}

async fn drive_aliases(
    addr: std::net::SocketAddr,
    cookie: String,
    hub: &remuda_hub::RunningHub,
    dir: &std::path::Path,
) {
    // Discover the online runtime host.
    let hosts = poll_json(addr, &cookie, "/v1/hosts", |value| {
        value["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|row| row["online"].as_bool() == Some(true))
        })
    })
    .await;
    let host_id = hosts["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["online"].as_bool() == Some(true))
        .and_then(|row| row["hostId"].as_str().or_else(|| row["id"].as_str()))
        .expect("online host id")
        .to_owned();

    // The REAL fs tree (item 5):
    //   other/proj       registered workspace B (real dir)
    //   allowed/proj     a DIFFERENT real dir A, unregistered
    //   allowed/link     real symlink -> other   (link/../proj => B)
    //   link2            real symlink -> other/proj
    //   "space "         registered workspace C whose name ends in a space
    let other_proj = dir.join("other/proj");
    let allowed_proj = dir.join("allowed/proj");
    std::fs::create_dir_all(&other_proj).unwrap();
    std::fs::create_dir_all(&allowed_proj).unwrap();
    #[cfg(unix)]
    {
        // link must point AT B (other/proj): resolve-symlink-then-`..` lands back in B
        std::os::unix::fs::symlink(&other_proj, dir.join("allowed/link")).unwrap();
        std::os::unix::fs::symlink(&other_proj, dir.join("link2")).unwrap();
    }
    let spaced = dir.join("space ");
    std::fs::create_dir_all(&spaced).unwrap();

    async fn register(
        addr: std::net::SocketAddr,
        cookie: &str,
        host_id: &str,
        path: &std::path::Path,
    ) -> Value {
        let (status, body) = http(
            addr,
            "POST",
            &format!("/v1/hosts/{host_id}/workspaces"),
            Some(cookie),
            Some(&json!({"path": path.display().to_string()}).to_string()),
        )
        .await;
        assert_eq!(status, 200, "register {}: {body}", path.display());
        serde_json::from_str(body.trim()).unwrap()
    }
    register(addr, &cookie, &host_id, &other_proj).await;
    register(addr, &cookie, &host_id, &spaced).await;

    let view = poll_json(
        addr,
        &cookie,
        &format!("/v1/hosts/{host_id}/workspaces"),
        |value| {
            value["workspaces"]
                .as_array()
                .is_some_and(|rows| rows.len() >= 3)
        },
    )
    .await;
    let rows = view["workspaces"].as_array().unwrap();
    let canonical =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap().display().to_string();
    let other_canonical = canonical(&other_proj);
    let spaced_canonical = canonical(&spaced);
    let find = |root: &str| {
        rows.iter()
            .find(|row| row["root"].as_str() == Some(root))
            .unwrap_or_else(|| panic!("workspace {root} in snapshot: {rows:?}"))
    };
    let other_id = find(&other_canonical)["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let spaced_id = find(&spaced_canonical)["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();

    // One active task on B and one on C; no sessions anywhere.
    hub.test_insert_bound_task("tsk_alias_b", "prj_alias", &host_id, &other_id, "running")
        .await
        .unwrap();
    hub.test_insert_bound_task("tsk_alias_c", "prj_alias", &host_id, &spaced_id, "running")
        .await
        .unwrap();

    async fn delete(
        addr: std::net::SocketAddr,
        cookie: &str,
        host_id: &str,
        path: &std::path::Path,
    ) -> (u16, String) {
        http(
            addr,
            "DELETE",
            &format!("/v1/hosts/{host_id}/workspaces"),
            Some(cookie),
            Some(&json!({"path": path.display().to_string()}).to_string()),
        )
        .await
    }

    // (1) symlink + `..`: realpath lands on registered B, whose active task
    //     must make the DELETE 409.
    let (status, body) = delete(addr, &cookie, &host_id, &dir.join("allowed/link/../proj")).await;
    assert_eq!(
        status, 409,
        "link/../proj realpaths to occupied workspace B: {body}"
    );
    assert!(body.contains("active task(s)"), "{body}");

    // (2) a plain symlink to B resolves identically.
    #[cfg(unix)]
    {
        let (status, body) = delete(addr, &cookie, &host_id, &dir.join("link2")).await;
        assert_eq!(status, 409, "symlink link2 resolves to B: {body}");
        assert!(body.contains("active task(s)"), "{body}");
    }

    // (3) the exact trailing-space root bytes name C and hit C's task.
    let (status, body) = delete(addr, &cookie, &host_id, &spaced).await;
    assert_eq!(status, 409, "trailing-space workspace C: {body}");
    assert!(body.contains("active task(s)"), "{body}");

    // (4) the same bytes WITHOUT the trailing space name nothing real: the
    //     production Node refuses resolution -> 400, no unregister.
    let trimmed = spaced.display().to_string();
    let trimmed = trimmed.trim_end();
    let (status, body) = http(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host_id}/workspaces"),
        Some(&cookie),
        Some(&json!({"path": trimmed}).to_string()),
    )
    .await;
    assert_eq!(status, 400, "trimmed spelling must not resolve: {body}");

    // (5) the LEXICAL sibling A the browse view would show is genuinely a
    //     different, unregistered directory: resolve refuses it too.
    let (status, body) = delete(addr, &cookie, &host_id, &allowed_proj).await;
    assert_eq!(
        status, 400,
        "allowed/proj is unregistered even though link/../proj lexically looks like it: {body}"
    );

    // (6) r7 item 2 REAL-DELETE success: archive the task occupying
    //     workspace C (archived tasks no longer count), then a
    //     component-equivalent spelling of its stored root (dot) must resolve
    //     through the production Node and SETTLE 200.
    hub.test_finish_bound_task("tsk_alias_c")
        .await
        .expect("finish task C");
    async fn still_listed(
        addr: std::net::SocketAddr,
        cookie: &str,
        host_id: &str,
        id: &str,
    ) -> bool {
        let (status, view) = http(
            addr,
            "GET",
            &format!("/v1/hosts/{host_id}/workspaces"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(status, 200, "{view}");
        let view: Value = serde_json::from_str(view.trim()).unwrap();
        view["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["workspaceId"].as_str() == Some(id))
    }

    // Only the first spelling can settle (the membership is gone after it);
    // the point is that an ALIAS spelling of the stored root reaches a 200
    // removal at all. Use `/./`-dot form against the real fs.
    let alias = spaced.join(".");
    let (status, body) = delete(addr, &cookie, &host_id, &alias).await;
    assert_eq!(
        status, 200,
        "dot alias of the unoccupied stored root must SETTLE (r7 item 2): {body}"
    );
    let settled: Value = serde_json::from_str(body.trim()).unwrap();
    assert!(
        settled["workspaces"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .all(|row| row["workspaceId"].as_str() != Some(spaced_id.as_str())),
        "dot-alias DELETE removed workspace C: {settled}"
    );
    assert!(
        !still_listed(addr, &cookie, &host_id, &spaced_id).await,
        "C gone after dot-alias DELETE"
    );

    // A repeated-separator spelling of the now-removed C names nothing (400),
    // proving the alias really resolved to C rather than a stale shortcut.
    let doubled = format!("{}//", spaced_canonical);
    let (status, body) = http(
        addr,
        "DELETE",
        &format!("/v1/hosts/{host_id}/workspaces"),
        Some(&cookie),
        Some(&json!({"path": doubled}).to_string()),
    )
    .await;
    assert_eq!(status, 400, "removed C no longer resolves: {body}");
}
