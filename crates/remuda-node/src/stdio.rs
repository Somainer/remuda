//! NDJSON `node --stdio` carrier: JSON-RPC `node.auth` / `node.hello` / `instance.*`.

use crate::enroll::{self, Enrollment};
use crate::inventory::{CollectRequest, collect};
use crate::transport::hubnode::{self as hubnode_codec, decode_request};
use crate::{DevNode, DevServerConfig, NodeError, NodeHello, ServeConfig, compose};
use remuda_protocol::hubnode::{HubNodeMethod, METHOD_JOURNAL_APPEND, METHOD_NODE_HELLO};
use remuda_protocol::{Id, InstanceId, JournalEvent, U64};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

const MAX_STDIO_FRAME_BYTES: usize = 1_048_576;
const JOURNAL_QUEUE_CAPACITY: usize = 128;

/// Options for [`run_stdio_opts`].
#[derive(Debug, Clone)]
pub struct StdioOptions {
    /// Placement labels (`region=sg`).
    pub labels: BTreeMap<String, String>,
    /// Advertised instance ceiling.
    pub max_instances: usize,
    /// Registry display name.
    pub display_label: Option<String>,
    /// Carrier advertised to Hub (`ssh-stdio`).
    pub transport: String,
    /// Data directory for `enrollment.json` and the Node journal.
    pub data_dir: PathBuf,
}

impl Default for StdioOptions {
    fn default() -> Self {
        Self {
            labels: BTreeMap::new(),
            max_instances: 8,
            display_label: None,
            transport: "ssh-stdio".into(),
            data_dir: enroll::default_data_dir(),
        }
    }
}

/// Serve inventory on stdout and accept Hub JSON-RPC frames on stdin.
pub async fn run_stdio(
    labels: BTreeMap<String, String>,
    max_instances: usize,
) -> Result<(), NodeError> {
    run_stdio_opts(StdioOptions {
        labels,
        max_instances,
        ..StdioOptions::default()
    })
    .await
}

/// Compose a durable FakeDriver runtime and serve it over stdio.
///
/// The `remuda node --stdio` composition root uses
/// [`run_stdio_runtime_opts`] with its native driver registry instead.
pub async fn run_stdio_opts(opts: StdioOptions) -> Result<(), NodeError> {
    let node = compose(&ServeConfig::fake(
        DevServerConfig::loopback(0),
        opts.data_dir.clone(),
    ))?;
    run_stdio_runtime_opts(node, opts).await
}

/// Serve an already-composed Node runtime over NDJSON stdin/stdout.
pub async fn run_stdio_runtime_opts(node: DevNode, opts: StdioOptions) -> Result<(), NodeError> {
    node.reconcile_herdr().await?;
    let result = tokio::select! {
        result = run_stdio_runtime(node.clone(), opts, None, None) => result,
        result = shutdown_signal() => result,
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), node.shutdown())
        .await
        .map_err(|_| {
            NodeError::Driver("Node shutdown deadline exceeded; ownership retained".into())
        })??;
    result
}

async fn shutdown_signal() -> Result<(), NodeError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// Alias of [`run_stdio_runtime_opts`] for the musl `remuda-node-stdio` bin.
pub async fn run_stdio_with_node(node: DevNode, opts: StdioOptions) -> Result<(), NodeError> {
    run_stdio_runtime_opts(node, opts).await
}

pub(crate) async fn run_stdio_runtime(
    node: DevNode,
    opts: StdioOptions,
    bootstrap_token: Option<String>,
    hello: Option<&NodeHello>,
) -> Result<(), NodeError> {
    let enrollment = enroll::load_or_create(&opts.data_dir)?;
    node.configure_doctor(crate::DoctorContext {
        data_dir: Some(opts.data_dir.clone()),
        listeners: Vec::new(),
    })?;
    ensure_runtime_identity(&node, &enrollment)?;
    let token = enrollment
        .node_token
        .clone()
        .or(bootstrap_token)
        .or_else(env_bootstrap_token);
    serve_stdio(
        node,
        opts,
        enrollment,
        token,
        hello,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_stdio<R, W>(
    node: DevNode,
    opts: StdioOptions,
    enrollment: Enrollment,
    token: Option<String>,
    hello: Option<&NodeHello>,
    input: R,
    mut output: W,
) -> Result<(), NodeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (node_epoch, mut host, default_label) = match hello {
        Some(hello) => (
            hello.params.node_epoch.clone(),
            serde_json::to_value(&hello.params.host)?,
            hello.params.host.hostname.clone(),
        ),
        None => {
            let snapshot = collect(&CollectRequest {
                labels: opts.labels.clone(),
                max_instances: opts.max_instances,
                herdr_socket: None,
            });
            let label = snapshot.hostname.clone();
            (Id::new("epoch")?, serde_json::to_value(snapshot)?, label)
        }
    };
    if let Some(object) = host.as_object_mut() {
        object.insert("hostId".into(), json!(enrollment.host_id));
    }
    node.advertise_workspaces(&mut host)?;
    let display_label = opts.display_label.as_deref().unwrap_or(&default_label);

    if let Some(token) = token.as_deref() {
        write_ndjson(&mut output, &hubnode_codec::encode_auth("auth-1", token)).await?;
    }
    let params = hubnode_codec::stdio_hello_params(
        &enrollment.host_id,
        display_label,
        &opts.transport,
        env!("CARGO_PKG_VERSION"),
        &node_epoch,
        host,
        token.as_deref(),
    );
    write_ndjson(
        &mut output,
        &hubnode_codec::encode_hello_request("hello-1", params),
    )
    .await?;

    let mut input = BufReader::new(input).lines();
    let (journal_tx, mut journal_rx) = mpsc::channel(JOURNAL_QUEUE_CAPACITY);
    let (tty_tx, mut tty_rx) = mpsc::channel(JOURNAL_QUEUE_CAPACITY);
    // Object pulls ride the carrier itself: a stdio-enrolled host usually has
    // no HTTP route to the Hub, and the ssh bridge forwards JSON only.
    let (carrier_tx, mut carrier_rx) = mpsc::channel(16);
    let object_broker = crate::carrier_objects::CarrierObjectBroker::new(carrier_tx);
    node.set_object_source(Arc::new(object_broker.source()));
    let (gate_tx, mut gate_rx) = mpsc::channel(JOURNAL_QUEUE_CAPACITY);
    // Long carrier methods park for minutes (a gate run/then/land/unpin, a
    // worker provision/remove that shells out to git and reclaims a cargo
    // target, an instance close, a worktree/SCM read): serving them inline on
    // the input arm freezes every outgoing pump below until the body ends. They
    // run on their own task through the same `handle_stdio_frame` entry point,
    // and the resulting frame (plus any journal pump the reply must start) rides
    // this channel back to the single writer, interleaving with gate.event, tty
    // and journal frames in any order. gate.cancel and every cheap read stay
    // inline so the cancel escape hatch fires at once.
    let (long_tx, mut long_rx) = mpsc::channel::<FrameOutcome>(JOURNAL_QUEUE_CAPACITY);
    spawn_stdio_tty_pump(node.clone(), tty_tx);
    spawn_stdio_gate_pump(node.clone(), gate_tx);
    let mut pumps = HashMap::<InstanceId, JoinHandle<()>>::new();

    loop {
        tokio::select! {
            frame = journal_rx.recv(), if !pumps.is_empty() => {
                if let Some(frame) = frame {
                    write_ndjson(&mut output, &frame).await?;
                }
            }
            frame = carrier_rx.recv() => {
                let Some(frame) = frame else { break; };
                write_ndjson(&mut output, &frame).await?;
            }
            frame = tty_rx.recv() => {
                if let Some(frame) = frame {
                    write_ndjson(&mut output, &frame).await?;
                }
            }
            frame = gate_rx.recv() => {
                if let Some(frame) = frame {
                    write_ndjson(&mut output, &frame).await?;
                }
            }
            frame = long_rx.recv() => {
                if let Some(outcome) = frame {
                    if let Some(response) = outcome.response {
                        write_ndjson(&mut output, &response).await?;
                    }
                    if let Some(instance_id) = outcome.pump_instance {
                        ensure_journal_pump(&node, instance_id, &journal_tx, &mut pumps)?;
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else {
                    break;
                };
                if line.len() > MAX_STDIO_FRAME_BYTES {
                    write_ndjson(
                        &mut output,
                        &hubnode_codec::rpc_error(
                            Value::Null,
                            -32600,
                            "NDJSON frame exceeds 1048576 bytes",
                        ),
                    )
                    .await?;
                    stop_pumps(pumps).await;
                    return Err(NodeError::InvalidRequest(
                        "stdio NDJSON frame exceeds local limit".to_owned(),
                    ));
                }
                if line.trim().is_empty() {
                    continue;
                }
                let frame: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => {
                        write_ndjson(
                            &mut output,
                            &hubnode_codec::rpc_error(Value::Null, -32700, &error.to_string()),
                        )
                        .await?;
                        continue;
                    }
                };
                // object.pull replies and object.chunk notifications complete
                // attachment fetches; they never become instance RPCs.
                if object_broker.handle_frame(&frame) {
                    continue;
                }
                // Long carrier methods can park for minutes: a gate
                // run/then/land/unpin, a worker.provision/worker.remove that
                // shells out to `git worktree` and recursively reclaims a cargo
                // target, an instance.close, or a worktree/SCM read that shells
                // out to git. Serving any of them inline here would starve every
                // outgoing pump in this select until the body returns — the
                // freeze the live demo saw as a job `running` with no steps, and
                // the same park a `remuda retire` produced with no gate at all.
                // Hand them to a task that runs the identical `handle_stdio_frame`
                // path off the loop and sends the outcome (reply + any journal
                // pump the reply starts) back through `long_tx`; every other
                // method keeps its inline path. gate.cancel is not long, so it
                // stays inline and can kill a running gate.
                if frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(crate::gate::is_long_carrier_method)
                {
                    let node = node.clone();
                    let enrollment = enrollment.clone();
                    let data_dir = opts.data_dir.clone();
                    let transport = opts.transport.clone();
                    let long_tx = long_tx.clone();
                    tokio::spawn(async move {
                        match handle_stdio_frame(
                            &node,
                            &enrollment,
                            &data_dir,
                            &transport,
                            frame,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                let _ = long_tx.send(outcome).await;
                            }
                            Err(error) => {
                                tracing::debug!(%error, "off-loop stdio dispatch failed");
                            }
                        }
                    });
                    continue;
                }
                let outcome = handle_stdio_frame(
                    &node,
                    &enrollment,
                    &opts.data_dir,
                    &opts.transport,
                    frame,
                )
                .await?;
                if let Some(response) = outcome.response {
                    write_ndjson(&mut output, &response).await?;
                }
                if let Some(instance_id) = outcome.pump_instance {
                    ensure_journal_pump(&node, instance_id, &journal_tx, &mut pumps)?;
                }
            }
        }
    }
    stop_pumps(pumps).await;
    object_broker.fail_all(NodeError::Disconnected);
    Ok(())
}

struct FrameOutcome {
    response: Option<Value>,
    pump_instance: Option<InstanceId>,
}

impl FrameOutcome {
    fn none() -> Self {
        Self {
            response: None,
            pump_instance: None,
        }
    }

    fn reply(response: Value) -> Self {
        Self {
            response: Some(response),
            pump_instance: None,
        }
    }
}

async fn handle_stdio_frame(
    node: &DevNode,
    enrollment: &Enrollment,
    data_dir: &Path,
    transport: &str,
    frame: Value,
) -> Result<FrameOutcome, NodeError> {
    if frame.get("method").is_none() {
        hubnode_codec::persist_hello_result(data_dir, &frame)?;
        return Ok(FrameOutcome::none());
    }
    let request = match decode_request(&frame) {
        Ok(request) => request,
        Err(_) if frame.get("type").is_some() => {
            return Ok(FrameOutcome::reply(reply_legacy(enrollment, &frame)));
        }
        Err(error) => {
            return Ok(FrameOutcome::reply(hubnode_codec::rpc_error(
                frame.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                &error.to_string(),
            )));
        }
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    let params = request.params.clone().unwrap_or(json!({}));
    let kind = request.method_kind();
    if kind.is_some_and(HubNodeMethod::is_hello) || request.method == METHOD_NODE_HELLO {
        return Ok(FrameOutcome {
            response: response_for(
                id,
                Ok(json!({
                    "ok": true,
                    "hostId": enrollment.host_id,
                    "carrier": transport,
                })),
            ),
            pump_instance: None,
        });
    }

    if crate::interactions::is_interaction_method(request.method.as_str()) {
        let pump_instance = instance_id_from_params(&params);
        let result = node
            .dispatch_interaction(request.method.as_str(), params)
            .await;
        return Ok(FrameOutcome {
            response: response_for(id, result),
            pump_instance,
        });
    }

    match kind {
        Some(method) if HubNodeMethod::is_instance(method) => {
            let target = instance_id_from_params(&params);
            let result =
                hubnode_codec::dispatch_method(node, request.method.as_str(), params).await;
            let pump_instance = result
                .as_ref()
                .ok()
                .and_then(instance_id_from_result)
                .or(target);
            Ok(FrameOutcome {
                response: response_for(id, result),
                pump_instance,
            })
        }
        Some(
            HubNodeMethod::NodeHeartbeat
            | HubNodeMethod::RuntimeHeartbeat
            | HubNodeMethod::JournalAppend
            | HubNodeMethod::TtyFrame,
        ) => {
            let result =
                hubnode_codec::dispatch_method(node, request.method.as_str(), params).await;
            Ok(FrameOutcome {
                response: response_for(id, result),
                pump_instance: None,
            })
        }
        _ if request.method == "instance.close" => {
            let pump_instance = instance_id_from_params(&params);
            let result =
                hubnode_codec::dispatch_method(node, request.method.as_str(), params).await;
            Ok(FrameOutcome {
                response: response_for(id, result),
                pump_instance,
            })
        }
        _ if request.method == "instance.purge" => {
            let result =
                hubnode_codec::dispatch_method(node, request.method.as_str(), params).await;
            Ok(FrameOutcome {
                response: response_for(id, result),
                pump_instance: None,
            })
        }
        _ if crate::workspace::is_workspace_method(request.method.as_str())
            || request.method == "host.doctor"
            || request.method == "host.resources"
            || crate::worktree::is_worktree_method(request.method.as_str())
            || crate::worker::is_worker_method(request.method.as_str())
            || crate::gate::is_gate_method(request.method.as_str())
            || crate::workspace_scm::is_scm_method(request.method.as_str())
            // Drill-in read of one subagent's sidechain transcript. Without
            // this row the ssh-stdio carrier refuses it, and the web row can
            // only say 「启动中」 forever.
            || crate::subagent::is_subagent_method(request.method.as_str()) =>
        {
            let result =
                hubnode_codec::dispatch_method(node, request.method.as_str(), params).await;
            Ok(FrameOutcome {
                response: response_for(id, result),
                pump_instance: None,
            })
        }
        _ => Ok(FrameOutcome {
            response: response_for(
                id,
                Err(NodeError::InvalidRequest(format!(
                    "stdio runtime does not handle {}",
                    request.method
                ))),
            ),
            pump_instance: None,
        }),
    }
}

fn response_for(id: Value, result: Result<Value, NodeError>) -> Option<Value> {
    if id.is_null() {
        return None;
    }
    Some(match result {
        Ok(result) => hubnode_codec::rpc_ok(id, result),
        Err(error) => hubnode_codec::rpc_error(id, rpc_code(&error), &error.to_string()),
    })
}

fn reply_legacy(enrollment: &Enrollment, frame: &Value) -> Value {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| frame.get("method").and_then(Value::as_str));
    match kind {
        Some("hub.hello") => json!({
            "type": "node.ready",
            "hostId": enrollment.host_id,
            "carrier": "stdio-ndjson",
        }),
        Some("hub.ping") => json!({
            "type": "node.pong",
            "id": frame.get("id").cloned().unwrap_or(Value::Null),
        }),
        Some(other) => hubnode_codec::rpc_error(
            frame.get("id").cloned().unwrap_or(Value::Null),
            -32601,
            &format!("stdio carrier does not handle {other}"),
        ),
        None => hubnode_codec::rpc_error(
            frame.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "NDJSON application frame requires type or method",
        ),
    }
}

pub(crate) fn spawn_stdio_tty_pump(node: DevNode, output: mpsc::Sender<Value>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = node.tty().subscribe();
        loop {
            let event = tokio::select! {
                _ = output.closed() => break,
                event = events.recv() => event,
            };
            match event {
                Ok(crate::TtyEvent::Open {
                    instance_id,
                    stream_id,
                }) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.frame",
                        "params": {
                            "instanceId": instance_id,
                            "streamId": stream_id,
                            "channel": 1,
                            "offset": "0",
                        }
                    });
                    if output.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(crate::TtyEvent::Mode {
                    instance_id,
                    stream_id,
                    alt_screen,
                }) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.mode",
                        "params": { "instanceId": instance_id, "streamId": stream_id, "altScreen": alt_screen }
                    });
                    if output.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(crate::TtyEvent::Progress {
                    instance_id,
                    stream_id,
                    progress,
                }) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.mode",
                        "params": {
                            "instanceId": instance_id,
                            "streamId": stream_id,
                            "progress": crate::tty::progress_json(progress),
                        }
                    });
                    if output.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(crate::TtyEvent::Bytes {
                    instance_id,
                    stream_id,
                    offset,
                    payload,
                }) => {
                    use base64::Engine;
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "tty.frame",
                        "params": {
                            "instanceId": instance_id,
                            "streamId": stream_id,
                            "channel": 1,
                            "offset": offset.to_string(),
                            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&payload),
                        }
                    });
                    if output.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Pump Node-originated `gate.event` notifications (batch 6 lane runner).
pub(crate) fn spawn_stdio_gate_pump(node: DevNode, output: mpsc::Sender<Value>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = node.gate_registry().subscribe();
        loop {
            let event = tokio::select! {
                _ = output.closed() => break,
                event = events.recv() => event,
            };
            match event {
                Ok(event) => {
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": remuda_protocol::METHOD_GATE_EVENT,
                        "params": event,
                    });
                    if output.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Start forwarding one Instance's journal, if there is one to forward.
///
/// Best-effort by construction, and that is load-bearing: a Hub routinely
/// names an Instance this Node does not know — a row left over from a previous
/// Node that has since been replaced, a `tty.screen` for a pane the Hub has not
/// reaped, a retire of a worker whose instance already left. The carrier loop
/// calls this with `?`, so returning `Err` here does not skip a pump: it
/// unwinds `serve_stdio` and the whole Node process exits. Live evidence: a
/// single such request killed the Node, after which journals stopped reaching
/// the Hub (`durableSeq` frozen) and every later RPC hung until the Hub refused
/// new ones with "too many in-flight node rpcs" — with no gate running and no
/// busy thread, because the process was simply gone.
///
/// So a missing or already-gone instance is not an error to report to the
/// caller: the request's own reply already carries not-found, and a pump is
/// only ever an optimisation that starts streaming. Log it and move on.
fn ensure_journal_pump(
    node: &DevNode,
    instance_id: InstanceId,
    output: &mpsc::Sender<Value>,
    pumps: &mut HashMap<InstanceId, JoinHandle<()>>,
) -> Result<(), NodeError> {
    pumps.retain(|_, task| !task.is_finished());
    if pumps.contains_key(&instance_id) {
        return Ok(());
    }
    let (receiver, journal_id) = match node
        .subscribe(&instance_id)
        .and_then(|receiver| Ok((receiver, node.get_instance(&instance_id)?.journal_id)))
    {
        Ok(parts) => parts,
        Err(error) => {
            tracing::debug!(%error, instance_id = %instance_id.as_id(), "no journal pump for an instance this Node does not know");
            return Ok(());
        }
    };
    let node = node.clone();
    let output = output.clone();
    let pump_id = instance_id.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = pump_journal(node, pump_id, journal_id, receiver, output).await {
            tracing::debug!(%error, "stdio journal pump stopped");
        }
    });
    pumps.insert(instance_id, task);
    Ok(())
}

async fn pump_journal(
    node: DevNode,
    instance_id: InstanceId,
    journal_id: Id,
    mut receiver: broadcast::Receiver<JournalEvent>,
    output: mpsc::Sender<Value>,
) -> Result<(), NodeError> {
    let mut last = forward_backlog(&node, &instance_id, &journal_id, U64(0), &output).await?;
    loop {
        match receiver.recv().await {
            Ok(event) => {
                if event.position().1 > last {
                    forward_event(&instance_id, &event, &output).await?;
                    last = event.position().1;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                last = forward_backlog(&node, &instance_id, &journal_id, last, &output).await?;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn forward_backlog(
    node: &DevNode,
    instance_id: &InstanceId,
    journal_id: &Id,
    mut last: U64,
    output: &mpsc::Sender<Value>,
) -> Result<U64, NodeError> {
    // Nothing to read when the caller is already at the journal's durable seq.
    // The check costs one watermark query and saves a page read that would
    // otherwise return empty.
    if node.journal_durable_seq(journal_id)? <= last {
        return Ok(last);
    }
    loop {
        let page = node.read_journal(journal_id, (last.0 > 0).then_some(last), 256)?;
        if page.events.is_empty() {
            return Ok(last);
        }
        let before = last;
        for event in page.events {
            if event.position().1 > last {
                forward_event(instance_id, &event, output).await?;
                last = event.position().1;
            }
        }
        if last == before || last >= page.durable_seq {
            return Ok(last);
        }
    }
}

async fn forward_event(
    instance_id: &InstanceId,
    event: &JournalEvent,
    output: &mpsc::Sender<Value>,
) -> Result<(), NodeError> {
    let seq = i64::try_from(event.position().1.0)
        .map_err(|_| NodeError::InvalidRequest("journal sequence exceeds i64".to_owned()))?;
    let params = hubnode_codec::encode_append(
        instance_id.as_id().as_str(),
        Some(seq),
        serde_json::to_value(event)?,
    );
    let frame = hubnode_codec::rpc_request(
        format!("journal-{}-{seq}", instance_id.as_id().as_str()),
        METHOD_JOURNAL_APPEND,
        params,
    );
    output
        .send(frame)
        .await
        .map_err(|_| NodeError::Disconnected)
}

async fn stop_pumps(pumps: HashMap<InstanceId, JoinHandle<()>>) {
    for (_, task) in pumps {
        task.abort();
        let _ = task.await;
    }
}

fn instance_id_from_params(params: &Value) -> Option<InstanceId> {
    params
        .get("instanceId")
        .or_else(|| params.pointer("/spec/instanceId"))
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse().ok())
}

fn instance_id_from_result(result: &Value) -> Option<InstanceId> {
    result
        .pointer("/instance/id")
        .or_else(|| result.pointer("/instance/instanceId"))
        .or_else(|| result.get("instanceId"))
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse().ok())
}

fn ensure_runtime_identity(node: &DevNode, enrollment: &Enrollment) -> Result<(), NodeError> {
    let runtime_host = node.host().meta.id;
    if runtime_host != enrollment.host_id {
        return Err(NodeError::InvalidConfig(format!(
            "runtime host {} does not match enrolled host {}",
            runtime_host.as_id(),
            enrollment.host_id.as_id(),
        )));
    }
    Ok(())
}

fn env_bootstrap_token() -> Option<String> {
    crate::enroll::enroll_token_from_env()
}

fn rpc_code(error: &NodeError) -> i64 {
    match error {
        NodeError::InvalidRequest(_) | NodeError::NotFound { .. } => -32602,
        NodeError::QueueFull => -32001,
        NodeError::DriverUnavailable => -32002,
        NodeError::InteractionExpired => -32005,
        NodeError::InteractionSuperseded { .. } => -32004,
        _ => -32603,
    }
}

async fn write_ndjson<W>(output: &mut W, value: &Value) -> Result<(), NodeError>
where
    W: AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    output.write_all(&encoded).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, duplex};
    use tokio::time::Duration;

    async fn read_json<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("line");
        serde_json::from_str(line.trim()).expect("json")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn composed_stdio_dispatches_create_and_streams_journal() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let opts = StdioOptions {
            data_dir: data_dir.path().to_path_buf(),
            display_label: Some("stdio-test".into()),
            transport: "ssh-stdio".into(),
            ..StdioOptions::default()
        };
        let node = compose(&ServeConfig::fake(
            DevServerConfig::loopback(0)
                .with_workspace_roots(remuda_testing::test_workspace_roots!()),
            opts.data_dir.clone(),
        ))
        .expect("compose");
        let enrollment = enroll::load_or_create(&opts.data_dir).expect("enrollment");
        // This tests stdio dispatch and journal streaming, independently of
        // personal CLI version/auth probes and their startup deadlines.
        let hello = NodeHello {
            jsonrpc: "2.0".into(),
            method: METHOD_NODE_HELLO.into(),
            params: crate::NodeHelloParams {
                node_epoch: Id::new("epoch").expect("epoch"),
                protocol: crate::NodeHelloProtocol {
                    major: 1,
                    minor: 0,
                    framing: "ndjson".into(),
                    max_frame_bytes: MAX_STDIO_FRAME_BYTES,
                },
                host: crate::HostInventory {
                    host_id: enrollment.host_id.clone(),
                    hostname: "stdio-fixture".into(),
                    driver_inventory: Vec::new(),
                    labels: BTreeMap::new(),
                    max_instances: opts.max_instances,
                    cli: Vec::new(),
                    herdr: crate::HerdrInventory {
                        absolute_path: None,
                        version: None,
                        socket: None,
                    },
                    resources: None,
                    os: None,
                    kernel: None,
                    libc: None,
                },
            },
        };
        let (client_in, node_out) = duplex(64 * 1024);
        let (node_in, mut client_out) = duplex(64 * 1024);
        let session_enrollment = enrollment.clone();
        let session = tokio::spawn(async move {
            serve_stdio(
                node,
                opts,
                session_enrollment,
                None,
                Some(&hello),
                node_in,
                node_out,
            )
            .await
        });

        let mut from_node = BufReader::new(client_in);
        let hello = tokio::time::timeout(Duration::from_secs(30), read_json(&mut from_node))
            .await
            .expect("hello deadline");
        assert_eq!(hello["method"], METHOD_NODE_HELLO);
        assert_eq!(hello["params"]["host"]["hostname"], "stdio-fixture");
        assert_eq!(
            hello["params"]["hostId"],
            json!(enrollment.host_id.as_id().as_str())
        );

        let create = hubnode_codec::rpc_request(
            "c-1",
            "instance.create",
            json!({
                "instanceId": InstanceId::new(),
                "kind": "claude",
                "driver": "claude-print",
                "prompt": "hi from stdio",
                "hostId": enrollment.host_id,
            }),
        );
        client_out
            .write_all(format!("{create}\n").as_bytes())
            .await
            .expect("write create");

        let mut saw_create = false;
        let mut saw_journal = false;
        for _ in 0..16 {
            let frame = tokio::time::timeout(Duration::from_secs(2), read_json(&mut from_node))
                .await
                .expect("frame deadline");
            if frame["id"] == "c-1" && frame.get("result").is_some() {
                saw_create = true;
            }
            if frame["method"] == METHOD_JOURNAL_APPEND {
                saw_journal = true;
            }
            if saw_create && saw_journal {
                break;
            }
        }
        assert!(saw_create, "instance.create result");
        assert!(saw_journal, "journal.append from stdio pump");
        session.abort();
    }
}
