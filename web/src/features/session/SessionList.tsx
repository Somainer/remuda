import { Link, useLocation, useSearchParams } from "react-router-dom";
import type { Instance, UiStatus } from "../../types/instance";
import { StateDot } from "../../components/StateDot";
import { nativeShort, projectStatus } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import ui from "../../styles/ui.module.css";

const GROUPS: { id: string; title: string; match: (s: UiStatus) => boolean }[] = [
  { id: "blocked", title: "待处理", match: (s) => s === "blocked" },
  { id: "working", title: "进行中", match: (s) => s === "working" || s === "starting" },
  { id: "recent", title: "最近", match: (s) => s === "idle" || s === "exited" || s === "unknown" },
];

function csv(params: URLSearchParams, key: string): string[] {
  return (params.get(key) ?? "").split(",").filter(Boolean);
}

function toggleCsv(params: URLSearchParams, key: string, value: string): URLSearchParams {
  const next = new URLSearchParams(params);
  const set = new Set(csv(params, key));
  if (set.has(value)) set.delete(value);
  else set.add(value);
  if (set.size) next.set(key, [...set].join(","));
  else next.delete(key);
  return next;
}

export function SessionList({ instances }: { instances?: Instance[] }) {
  const hub = useHub();
  const location = useLocation();
  const [params, setParams] = useSearchParams();
  const q = (params.get("q") ?? "").trim().toLowerCase();
  const statusFilter = csv(params, "status");
  const hostFilter = csv(params, "host");
  const workspaceFilter = csv(params, "workspace");
  const kindFilter = csv(params, "kind");
  const source = (instances ?? hub.instances).filter((i) => i.parent == null);

  const filtered = source.filter((instance) => {
    const status = projectStatus(instance);
    if (statusFilter.length && !statusFilter.includes(status)) return false;
    if (hostFilter.length && !hostFilter.includes(instance.hostId)) return false;
    if (workspaceFilter.length && !workspaceFilter.includes(instance.workspaceId)) return false;
    if (kindFilter.length && !kindFilter.includes(instance.kind)) return false;
    if (!q) return true;
    const title = hubStore.titleOf(instance.id).toLowerCase();
    const cwd = hubStore.workspaceOf(instance.workspaceId)?.rootPath.toLowerCase() ?? "";
    const native = instance.nativeRef.sessionId.state === "known" ? instance.nativeRef.sessionId.value.toLowerCase() : "";
    return title.includes(q) || cwd.includes(q) || native.includes(q) || instance.id.toLowerCase().includes(q);
  });

  if (hub.hosts.length === 0) {
    return (
      <p className={ui.listMeta} style={{ padding: 16 }}>
        无主机。<Link to="/hosts">添加主机</Link>
      </p>
    );
  }
  if (hub.instances.length === 0) {
    return (
      <p className={ui.listMeta} style={{ padding: 16 }}>
        还没有会话。<Link to="/sessions/new">新建会话</Link>
      </p>
    );
  }

  return (
    <div data-testid="session-list">
      <input
        className={ui.input}
        style={{ margin: "8px 12px", width: "calc(100% - 24px)" }}
        placeholder="搜索标题 / cwd / 原生 id"
        value={params.get("q") ?? ""}
        onChange={(e) => {
          const next = new URLSearchParams(params);
          if (e.target.value) next.set("q", e.target.value);
          else next.delete("q");
          setParams(next);
        }}
      />
      <div className={ui.row} style={{ padding: "0 12px 8px" }}>
        {(["blocked", "working", "starting", "idle", "exited"] as const).map((s) => (
          <button
            key={s}
            className={`${ui.chip} ${statusFilter.includes(s) ? ui.chipOn : ""}`}
            onClick={() => setParams(toggleCsv(params, "status", s))}
          >
            {s}
          </button>
        ))}
      </div>
      <div className={ui.row} style={{ padding: "0 12px 8px" }}>
        {hub.hosts.map((h) => (
          <button
            key={h.id}
            className={`${ui.chip} ${hostFilter.includes(h.id) ? ui.chipOn : ""}`}
            onClick={() => setParams(toggleCsv(params, "host", h.id))}
          >
            {h.label}
          </button>
        ))}
        {hub.workspaces.map((w) => (
          <button
            key={w.id}
            className={`${ui.chip} ${workspaceFilter.includes(w.id) ? ui.chipOn : ""}`}
            onClick={() => setParams(toggleCsv(params, "workspace", w.id))}
          >
            {w.label}
          </button>
        ))}
        {(["claude", "codex", "grok", "agy"] as const).map((k) => (
          <button
            key={k}
            className={`${ui.chip} ${kindFilter.includes(k) ? ui.chipOn : ""}`}
            onClick={() => setParams(toggleCsv(params, "kind", k))}
          >
            {k}
          </button>
        ))}
      </div>
      {GROUPS.map((group) => {
        const items = filtered.filter((i) => group.match(projectStatus(i)));
        if (!items.length) return null;
        return (
          <section key={group.id} data-testid={`session-group-${group.id}`}>
            <div className={ui.groupTitle}>
              {group.title} ({items.length})
            </div>
            {items.map((instance) => {
              const status = projectStatus(instance);
              const pending = hub.interactions.find((i) => i.instanceId === instance.id && i.state === "pending");
              const to = `/s/${instance.id}`;
              const active = location.pathname === to;
              const summary =
                pending?.request.kind === "approval"
                  ? `等你批准 ${pending.request.title}`
                  : pending?.request.kind === "question"
                    ? `AskUserQuestion · ${pending.request.fields.length} 题`
                    : hubStore.summaryOf(instance.id) ?? status;
              const workspace = hubStore.workspaceOf(instance.workspaceId);
              return (
                <Link
                  key={instance.id}
                  to={to}
                  className={`${ui.listItem} ${active ? ui.listItemActive : ""}`}
                  data-testid="session-row"
                  data-status={status}
                >
                  <StateDot status={status} />
                  <span>
                    <div>{hubStore.titleOf(instance.id)}</div>
                    <div className={ui.listMeta}>
                      {status} · {hubStore.hostName(instance.hostId)} · {workspace?.label} · {instance.kind} · {nativeShort(instance)}
                    </div>
                    <div className={ui.listMeta}>{summary}</div>
                  </span>
                </Link>
              );
            })}
          </section>
        );
      })}
    </div>
  );
}
