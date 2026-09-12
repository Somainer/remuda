import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { Button } from "../components/Button";
import { AddHostForm, hostRegistry, useHostViews, type HostView } from "../features/hosts";
import { useHub } from "../lib/store";
import ui from "../styles/ui.module.css";
import css from "../features/hosts/hosts.module.css";

export function HostsPage() {
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const [adding, setAdding] = useState(false);

  return (
    <div className={css.page} data-testid="hosts-page">
      <header className={css.head}>
        <h1 style={{ fontSize: 18, margin: 0 }}>主机</h1>
        <div className={ui.row}>
          <Link to="/fleet">Fleet</Link>
          <Button data-testid="hosts-add" onClick={() => setAdding(true)}>
            添加
          </Button>
        </div>
      </header>
      {hosts.map((host) => (
        <Link
          key={host.id}
          to={`/hosts/${host.id}`}
          className={ui.listItem}
          data-testid="host-row"
          data-transport={host.transport}
          data-online={host.online ? "1" : "0"}
          data-label={host.label}
        >
          <span className={`${css.dot} ${host.online ? css.dotOn : css.dotOff}`} aria-label={host.online ? "在线" : "离线"} />
          <span>
            <div>{host.label}</div>
            <div className={ui.listMeta}>
              {host.online ? "在线" : "离线"}
              {host.rttMs != null && host.online ? `  ${host.rttMs}ms` : "  —"}
              {"  "}
              {host.cli.map((c) => c.kind).join(" · ") || "—"}
              {"  "}
              会话 {host.instanceCount}
            </div>
            <div className={ui.listMeta}>
              {host.transport}
              {host.labels.length ? ` · ${host.labels.join(" ")}` : ""}
              {` · max ${host.maxInstances}`}
            </div>
          </span>
        </Link>
      ))}
      <AddHostForm open={adding} onClose={() => setAdding(false)} />
    </div>
  );
}

export function HostDetailPage() {
  const { hostId = "" } = useParams();
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const host = hosts.find((h) => h.id === hostId);
  if (!host) return <p style={{ padding: 16 }}>主机不存在</p>;
  const workspaces = hub.workspaces.filter((w) => w.hostId === host.id);
  return <HostDetail host={host} workspaceLabels={workspaces.map((w) => `${w.label} ${w.rootPath}`)} />;
}

function HostDetail({ host, workspaceLabels }: { host: HostView; workspaceLabels: string[] }) {
  const [label, setLabel] = useState(host.label);
  const [labelDraft, setLabelDraft] = useState(host.labels.join(", "));
  const [maxInstances, setMaxInstances] = useState(String(host.maxInstances));

  return (
    <div className={css.page} data-testid="host-detail">
      <p>
        <Link to="/hosts">← 主机</Link>
      </p>
      <h1>{host.label}</h1>
      <p className={ui.listMeta}>
        {host.online ? "在线" : "离线"} · 传输 {host.transport}
        {host.hostname ? ` · ${host.hostname}` : ""}
        {host.port ? `:${host.port}` : ""}
      </p>
      <p className={ui.listMeta}>
        Node Agent {host.agentVersion ?? "—"}
        {host.rttMs != null ? ` · ${host.rttMs}ms` : ""}
        {host.resources ? ` · cpu ${host.resources.cpuPct}% mem ${host.resources.memPct}%` : ""}
      </p>
      {!host.online ? <p className={ui.listMeta}>主机离线，不能 create/send；历史会话仍可读。</p> : null}
      <h2 style={{ fontSize: 14 }}>CLI</h2>
      {host.cli.length === 0 ? <p className={ui.listMeta}>尚无盘点</p> : null}
      {host.cli.map((cli) => (
        <p key={`${cli.kind}:${cli.path}`} className={css.cli} data-testid="host-cli">
          {cli.kind}  {cli.path}  {cli.version}  {cli.auth}
        </p>
      ))}
      <h2 style={{ fontSize: 14 }}>Workspace</h2>
      {workspaceLabels.length ? workspaceLabels.map((row) => (
        <p key={row} className={ui.listMeta}>
          {row}
        </p>
      )) : (
        <p className={ui.listMeta}>最近 cwd —</p>
      )}
      <label className={ui.field} style={{ marginTop: 12 }}>
        显示名
        <input className={ui.input} value={label} data-testid="host-label-edit" onChange={(e) => setLabel(e.target.value)} />
      </label>
      <label className={ui.field} style={{ marginTop: 8 }}>
        标签（逗号分隔）
        <input className={ui.input} value={labelDraft} data-testid="host-labels-edit" onChange={(e) => setLabelDraft(e.target.value)} />
      </label>
      <label className={ui.field} style={{ marginTop: 8 }}>
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
      <div className={ui.row} style={{ marginTop: 12 }}>
        <Button
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
        </Button>
        {host.online ? (
          <Link to={`/sessions/new?host=${host.id}`}>
            <Button variant="primary">新会话</Button>
          </Link>
        ) : (
          <Button disabled>新会话</Button>
        )}
      </div>
    </div>
  );
}
