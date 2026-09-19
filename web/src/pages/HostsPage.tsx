import { Fragment, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  AddHostForm,
  COMPUTER_USE_KIND,
  HostLaunchDefaults,
  HostProviderBinding,
  absentCli,
  cliSummary,
  hostRegistry,
  installedCli,
  isStaleOffline,
  useHostViews,
  type HostView,
} from "../features/hosts";
import { fromHub, type ProviderProfile } from "../features/providers";
import { hubStore, useHub } from "../lib/store";
import { api } from "../lib/api";
import { HostDiagnostics } from "../features/hosts/HostDiagnostics";
import { WorkspaceList } from "../features/workspaces/WorkspaceList";
import type { Workspace } from "../types/workspace";
import ui from "../styles/ui.module.css";
import css from "../features/hosts/hosts.module.css";

function useHostPolling() {
  useEffect(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const refresh = async () => {
      try { await hubStore.refreshHosts(); } catch { /* preserve last snapshot */ }
      if (!stopped) timer = setTimeout(() => void refresh(), 2500);
    };
    void refresh();
    return () => { stopped = true; clearTimeout(timer); };
  }, []);
}

export function HostsPage() {
  useHostPolling();
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const [adding, setAdding] = useState(false);
  const [showStale, setShowStale] = useState(false);
  const stale = hosts.filter((h) => isStaleOffline(h));
  const visible = showStale ? hosts : hosts.filter((h) => !isStaleOffline(h));
  const online = hosts.filter((h) => h.online).length;

  return (
    <div className={css.board} data-testid="hosts-page">
      <header className={css.head} data-testid="hosts-head">
        <h1 className={css.title}>主机</h1>
        <div className={css.headMeta}>
          <div className={css.count}>
            {visible.length} 台 · {online} 在线
          </div>
          <Link to="/fleet" className={css.count}>
            Fleet
          </Link>
          {stale.length > 0 ? (
            <button
              type="button"
              className={`${css.toggle} ${showStale ? css.toggleOn : ""}`}
              data-testid="hosts-show-stale"
              onClick={() => setShowStale((on) => !on)}
            >
              {showStale ? "隐藏过期" : `显示过期 (${stale.length})`}
            </button>
          ) : null}
        </div>
        <button type="button" className={css.add} data-testid="hosts-add" onClick={() => setAdding(true)}>
          添加主机
        </button>
      </header>
      {visible.map((host) => (
        <Fragment key={host.id}>
        <Link
          to={`/hosts/${host.id}`}
          className={css.row}
          data-testid="host-row"
          data-transport={host.transport}
          data-online={host.online ? "1" : "0"}
          data-stale={isStaleOffline(host) ? "1" : "0"}
          data-label={host.label}
        >
          <span className={`${css.dot} ${host.online ? css.dotOn : css.dotOff}`} aria-label={host.online ? "在线" : "离线"} />
          <span className={css.identity}>
            <span className={`${css.name} ${host.online ? "" : css.nameOff}`}>{host.label}</span>
            {host.lastError ? <span role="status" className={css.sshError}>{host.lastError}</span> : null}
            <span className={css.mobileMeta}>
              {host.state === "connecting" ? "连接中…" : host.online
                ? `在线${host.rttMs != null ? ` ${host.rttMs}ms` : ""}`
                : `离线${host.lastSeenAt ? ` · 最后心跳 ${host.lastSeenAt.slice(11, 16)}` : ""}`}
              {cliSummary(host.cli) ? ` · ${cliSummary(host.cli)}` : ""}
              {` · 会话 ${host.instanceCount}`}
            </span>
          </span>
          <span className={`${css.cell} ${css.cellRtt}`}>
            {host.state === "connecting" ? "连接中…" : host.online ? `在线${host.rttMs != null ? ` ${host.rttMs}ms` : ""}` : "离线 —"}
          </span>
          <span className={`${css.cell} ${css.cellCli}`}>{cliSummary(host.cli) || "—"}</span>
          <span className={`${css.cell} ${css.cellTransport}`}>
            {host.transport}
            {!host.online && host.lastSeenAt ? ` · 最后心跳 ${host.lastSeenAt.slice(11, 16)}` : ""}
          </span>
          <span className={`${css.cell} ${css.cellSessions}`}>会话 {host.instanceCount}</span>
          <span className={css.chevron} aria-hidden>
            ›
          </span>
        </Link>
        <WorkspaceList hostId={host.id} online={host.online} workspaces={hub.workspaces.filter((w) => w.hostId === host.id)} />
        </Fragment>
      ))}
      <AddHostForm open={adding} onClose={() => setAdding(false)} />
    </div>
  );
}

export function HostDetailPage() {
  useHostPolling();
  const { hostId = "" } = useParams();
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const host = hosts.find((h) => h.id === hostId);
  if (!host) return <p style={{ padding: 16 }}>主机不存在</p>;
  const workspaces = hub.workspaces.filter((w) => w.hostId === host.id);
  return <HostDetail host={host} workspaces={workspaces} />;
}

function HostDetail({ host, workspaces }: { host: HostView; workspaces: Workspace[] }) {
  const navigate = useNavigate();
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);
  const [label, setLabel] = useState(host.label);
  const [labelDraft, setLabelDraft] = useState(host.labels.join(", "));
  const [maxInstances, setMaxInstances] = useState(String(host.maxInstances));
  const [profiles, setProfiles] = useState<ProviderProfile[]>([]);
  const [bindingError, setBindingError] = useState<string | null>(null);
  const [launchError, setLaunchError] = useState<string | null>(null);

  useEffect(() => {
    void api
      .providerList({ hostId: host.id })
      .then((page) => setProfiles(page.items.map(fromHub)))
      .catch(() => setProfiles([]));
  }, [host.id]);

  return (
    <div className={css.board} data-testid="host-detail">
      <div className={css.detail}>
        <div className={css.detailHead}>
          <Link to="/hosts" className={css.back} aria-label="返回主机列表">
            ←
          </Link>
          <h1 className={css.detailTitle}>{host.label}</h1>
          <span className={css.detailMeta}>
            {host.id.slice(0, 10)} · Node Agent {host.agentVersion ?? "—"}
          </span>
          {host.online ? (
            <Link to={`/sessions/new?host=${host.id}`} className={css.newSession}>
              新会话
            </Link>
          ) : (
            <button type="button" className={css.newSession} disabled>
              新会话
            </button>
          )}
        </div>
        {host.ssh ? <div className={css.sshPanel}>
          <span>SSH · {host.ssh.target} · {host.state === "connecting" ? "连接中…" : host.online ? "在线" : "离线，自动重连中"}</span>
          <button type="button" className={css.add} disabled={removing} onClick={() => {
            setRemoving(true); setRemoveError(null);
            void api.hostRemove(host.id).then(async () => { await hubStore.refreshHosts(); navigate("/hosts"); })
              .catch((error: unknown) => setRemoveError(error instanceof Error ? error.message : "移除失败"))
              .finally(() => setRemoving(false));
          }} data-testid="host-remove">{removing ? "移除中…" : "移除主机"}</button>
          {host.lastError ? <p role="status" className={css.sshError}>{host.lastError}</p> : null}
          {removeError ? <p role="alert" className={css.sshError}>{removeError}</p> : null}
        </div> : null}
        <div className={css.metrics}>
          <div className={css.metric}>
            <div className={css.metricLabel}>传输</div>
            <div className={css.metricValue}>{host.transport}</div>
          </div>
          <div className={css.metric}>
            <div className={css.metricLabel}>心跳</div>
            <div className={css.metricValue}>{host.online ? `${host.rttMs ?? "—"}ms` : "离线"}</div>
          </div>
          {host.resources ? (
            <>
              {host.resources.cpuPct != null ? (
                <div
                  className={css.metric}
                  title={
                    host.resources.sampledAt
                      ? `资源采样于 ${host.resources.sampledAt}（超 60s 会在放置前向节点重新采样）`
                      : undefined
                  }
                >
                  <div className={css.metricLabel}>cpu</div>
                  <div className={css.metricValue}>{host.resources.cpuPct}%</div>
                </div>
              ) : null}
              {host.resources.memPct != null ? (
                <div
                  className={css.metric}
                  title={
                    host.resources.sampledAt
                      ? `资源采样于 ${host.resources.sampledAt}（超 60s 会在放置前向节点重新采样）`
                      : undefined
                  }
                >
                  <div className={css.metricLabel}>mem</div>
                  <div className={css.metricValue}>{host.resources.memPct}%</div>
                </div>
              ) : null}
            </>
          ) : null}
        </div>
        <div>
          <div className={css.sectionLabel}>CLI · 按本机盘点，绝对路径 + 版本</div>
          <div className={css.cliTable}>
            {installedCli(host.cli).length === 0 && absentCli(host.cli).length === 0 ? (
              <div className={css.cliRow}>尚无盘点</div>
            ) : null}
            {installedCli(host.cli).map((cli) => (
              <div key={`${cli.kind}:${cli.path}`} className={css.cliRow} data-testid="host-cli">
                <span className={css.cliKind}>
                  {cli.kind}
                  <span className={css.cliVerInline}>{cli.version ? ` ${cli.version}` : ""}</span>
                </span>
                <span className={css.cliPath}>{cli.path}</span>
                <span className={css.cliVer}>{cli.version}</span>
                <span className={css.cliAuth}>
                  <span className={cli.auth === "logged_in" || cli.auth === "gateway-native" ? css.dotAuth : css.dotUnknown} />
                  {cli.auth}
                </span>
                <span className={css.cliVer} data-testid="host-cli-flags">
                  已安装
                  {cli.kind === "claude" ? ` · nativeGateway ${cli.nativeGateway || cli.auth === "gateway-native" ? "true" : "false"}` : ""}
                </span>
              </div>
            ))}
            {/* A reported-absent row has no path to print, so `installedCli`
                (correctly) drops it; the capability still needs a row here, or
                "未安装" is unreachable and the host reads as if it never
                answered (ui-spec §2.6).
                Scoped to the capability on purpose: the Node probe reports
                every agent CLI it looked for, so an un-scoped list would give
                a claude-only host five empty 未安装 rows for codex/grok/agy/
                gemini — noise about binaries this page never claimed to have. */}
            {absentCli(host.cli)
              .filter((cli) => cli.kind === COMPUTER_USE_KIND)
              .map((cli) => (
                <div key={`${cli.kind}:absent`} className={css.cliRow} data-testid="host-cli">
                  <span className={css.cliKind}>{cli.kind}</span>
                  <span className={css.cliPath} />
                  <span className={css.cliVer} />
                  <span className={css.cliAuth}>
                    <span className={css.dotUnknown} />
                    {cli.auth}
                  </span>
                  <span className={css.cliVer} data-testid="host-cli-flags">未安装</span>
                </div>
              ))}
          </div>
        </div>
        <WorkspaceList hostId={host.id} workspaces={workspaces} online={host.online} />
        <HostDiagnostics hostId={host.id} online={host.online} cli={host.cli} />
        <div className={css.fields}>
          <label className={ui.field}>
            显示名
            <input className={ui.input} value={label} data-testid="host-label-edit" onChange={(e) => setLabel(e.target.value)} />
          </label>
          <label className={ui.field}>
            标签（逗号分隔）
            <input className={ui.input} value={labelDraft} data-testid="host-labels-edit" onChange={(e) => setLabelDraft(e.target.value)} />
          </label>
          <HostProviderBinding
            binding={host.providerBinding ?? "auto"}
            profiles={profiles.map((p) => ({ id: p.id, name: p.name, scope: p.scope }))}
            onChange={(providerBinding) => {
              setBindingError(null);
              hostRegistry.patch(host.id, { providerBinding });
              void api.hostPatch(host.id, { providerBinding }).catch((error: unknown) => {
                setBindingError(error instanceof Error ? error.message : "绑定失败");
              });
            }}
          />
          {bindingError ? <p role="alert" className={css.sshError}>{bindingError}</p> : null}
          <HostLaunchDefaults
            args={host.defaultLaunchArgs}
            binaryPath={host.claudeBinaryPath}
            tui={host.defaultTui}
            probedBinaryPath={host.cli?.find((entry) => entry.kind === "claude")?.path ?? undefined}
            onSave={(patch) => {
              setLaunchError(null);
              hostRegistry.patch(host.id, {
                defaultLaunchArgs: patch.defaultLaunchArgs ?? undefined,
                claudeBinaryPath: patch.claudeBinaryPath ?? undefined,
                ...(patch.defaultTui !== undefined ? { defaultTui: patch.defaultTui ?? undefined } : {}),
              });
              void api.hostPatch(host.id, patch).catch((error: unknown) => {
                setLaunchError(error instanceof Error ? error.message : "保存失败");
              });
            }}
          />
          {launchError ? <p role="alert" className={css.sshError}>{launchError}</p> : null}
          <label className={ui.field}>
            maxInstances
            <input
              className={ui.input}
              type="number"
              min={1}
              value={maxInstances}
              data-testid="host-max-instances"
              onChange={(e) => setMaxInstances(e.target.value)}
            />
          </label>
          <button
            type="button"
            className={css.add}
            data-testid="host-save-meta"
            onClick={() => {
              hostRegistry.patch(host.id, {
                label: label.trim() || host.label,
                labels: labelDraft
                  .split(",")
                  .map((item) => item.trim())
                  .filter(Boolean),
                maxInstances: Math.max(1, Number(maxInstances) || host.maxInstances),
              });
            }}
          >
            保存
          </button>
        </div>
        <div className={css.foot}>离线主机仍可看历史会话（journal 在 Hub），但不能 create / send</div>
      </div>
    </div>
  );
}
