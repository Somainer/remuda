import { Link, useLocation, useSearchParams } from "react-router-dom";
import type { Instance, UiStatus } from "../../types/instance";
import { StateDot } from "../../components/StateDot";
import { formatListTime, shortId } from "../../lib/format";
import { nativeShort, projectStatus, uiMode } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import css from "./SessionList.module.css";

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

function exitLabel(instance: Instance): string | null {
  if (instance.exit.state !== "known") return null;
  const code = instance.exit.value.code;
  return code == null ? "exit" : `exit ${code}`;
}

function pendingBadge(kind: string | undefined, title: string | undefined, fields: number | undefined): string | null {
  if (kind === "approval") return `等你批准 ${title ?? ""}`.trim();
  if (kind === "question") return `AskUserQuestion · ${fields ?? 0} 题`;
  if (kind === "plan-review") return "计划待审";
  return null;
}

export function SessionList({ instances, variant = "full" }: { instances?: Instance[]; variant?: "full" | "compact" }) {
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

  const filterCount = [hostFilter, workspaceFilter, kindFilter, statusFilter].filter((f) => f.length).length;
  const share = params.toString();

  if (hub.hosts.length === 0) {
    return (
      <p className={css.empty} data-testid="session-list">
        无主机。<Link to="/hosts">添加主机</Link>
      </p>
    );
  }
  if (hub.instances.length === 0) {
    return (
      <p className={css.empty} data-testid="session-list">
        还没有会话。<Link to="/sessions/new">新建会话</Link>
      </p>
    );
  }

  const live = hub.connection === "live";

  if (variant === "compact") {
    return (
      <div className={css.root} data-testid="session-list">
        <header className={css.compactTop}>
          <div className={css.compactTitle}>会话</div>
          {hostFilter.length ? <div className={css.compactHint}>host:{hostFilter.length}</div> : null}
        </header>
        {filtered.map((instance) => {
          const status = projectStatus(instance);
          const workspace = hubStore.workspaceOf(instance.workspaceId);
          const to = `/s/${instance.id}`;
          const active = location.pathname === to || location.pathname.startsWith(`${to}/`);
          return (
            <Link
              key={instance.id}
              to={to}
              className={`${css.compactRow} ${active ? css.compactRowActive : ""}`}
              data-testid="session-row"
              data-status={status}
            >
              <div className={css.compactHead}>
                <StateDot status={status} />
                <span className={`${css.compactName} ${status === "exited" || status === "unknown" ? css.compactNameMute : ""}`}>
                  {hubStore.titleOf(instance.id)}
                </span>
              </div>
              <div className={css.compactMeta}>
                {hubStore.hostName(instance.hostId)} / {workspace?.label} · {formatListTime(instance.updatedAt)}
              </div>
            </Link>
          );
        })}
      </div>
    );
  }

  return (
    <div className={css.root} data-testid="session-list">
      <header className={css.top}>
        <div className={css.title}>会话</div>
        <div className={css.count}>
          {source.length} 个实例 · {hub.hosts.length} 台主机
        </div>
        <div className={css.live}>
          <span className={`${css.liveDot} ${live ? "" : css.liveOff}`} />
          {live ? "live" : hub.connection}
        </div>
        <button type="button" className={css.filterBtn} aria-label="筛选">
          筛选{filterCount ? <span className={css.filterCount}>{filterCount}</span> : null}
        </button>
        <Link className={css.newBtn} to="/sessions/new">
          ＋ 新建
        </Link>
      </header>
      <div className={css.toolbar}>
        <label className={css.search}>
          <span className={css.searchGlyph}>⌕</span>
          <input
            className={css.searchInput}
            placeholder="搜索标题 / cwd / 原生 id"
            value={params.get("q") ?? ""}
            onChange={(e) => {
              const next = new URLSearchParams(params);
              if (e.target.value) next.set("q", e.target.value);
              else next.delete("q");
              setParams(next);
            }}
          />
        </label>
        <div className={css.chips}>
          {hub.hosts.map((h) => (
            <button
              key={h.id}
              type="button"
              className={`${css.chip} ${hostFilter.includes(h.id) ? css.chipOn : ""}`}
              onClick={() => setParams(toggleCsv(params, "host", h.id))}
            >
              {hostFilter.includes(h.id) ? (
                <>
                  <span className={css.chipKey}>host:</span>
                  {h.label}
                  <span className={css.chipX}>✕</span>
                </>
              ) : (
                h.label
              )}
            </button>
          ))}
          {hub.workspaces.map((w) => (
            <button
              key={w.id}
              type="button"
              className={`${css.chip} ${workspaceFilter.includes(w.id) ? css.chipOn : ""}`}
              onClick={() => setParams(toggleCsv(params, "workspace", w.id))}
            >
              {workspaceFilter.includes(w.id) ? `${w.label} ✕` : `${w.label} ▾`}
            </button>
          ))}
          {(["claude", "codex", "grok", "agy"] as const).map((k) => (
            <button
              key={k}
              type="button"
              className={`${css.chip} ${kindFilter.includes(k) ? css.chipOn : ""}`}
              onClick={() => setParams(toggleCsv(params, "kind", k))}
            >
              {kindFilter.includes(k) ? (
                <>
                  <span className={css.chipKey}>kind:</span>
                  {k}
                  <span className={css.chipX}>✕</span>
                </>
              ) : (
                k
              )}
            </button>
          ))}
          {(["blocked", "working", "starting", "idle", "exited"] as const).map((s) => (
            <button
              key={s}
              type="button"
              className={`${css.chip} ${statusFilter.includes(s) ? css.chipOn : ""}`}
              onClick={() => setParams(toggleCsv(params, "status", s))}
            >
              {statusFilter.includes(s) ? `${s} ✕` : s}
            </button>
          ))}
          {share ? <span className={css.share}>?{share} · 可分享</span> : null}
        </div>
      </div>
      {GROUPS.map((group) => {
        const items = filtered.filter((i) => group.match(projectStatus(i)));
        if (!items.length) return null;
        return (
          <section key={group.id} data-testid={`session-group-${group.id}`}>
            <div className={css.group}>
              <div className={`${css.groupTitle} ${group.id === "blocked" ? css.groupTitleBlocked : ""}`}>{group.title}</div>
              <div className={css.groupMeta}>
                {items.length}
                {group.id === "blocked" ? " · 置顶，不进折叠组" : group.id === "recent" ? " · 子 agent 不在此列" : ""}
              </div>
            </div>
            {items.map((instance) => {
              const status = projectStatus(instance);
              const pending = hub.interactions.find((i) => i.instanceId === instance.id && i.state === "pending");
              const to = `/s/${instance.id}`;
              const workspace = hubStore.workspaceOf(instance.workspaceId);
              const badge = pendingBadge(
                pending?.request.kind,
                pending && pending.request.kind !== "elicitation" ? pending.request.title : undefined,
                pending?.request.kind === "question" ? pending.request.fields.length : undefined,
              );
              const summary = hubStore.summaryOf(instance.id);
              const tty = uiMode(instance) === "tty-attachable";
              const compactRecent = status === "idle" || status === "exited" || status === "unknown";
              const title = hubStore.titleOf(instance.id);
              return (
                <Link
                  key={instance.id}
                  to={to}
                  className={`${css.row} ${status === "blocked" ? css.rowBlocked : ""} ${compactRecent ? css.rowIdle : ""}`}
                  data-testid="session-row"
                  data-status={status}
                >
                  <StateDot status={status} />
                  <span className={`${css.body} ${compactRecent ? css.bodyIdle : ""}`}>
                    {compactRecent ? (
                      <div className={css.idleMeta}>
                        <span>{status}</span>
                        <span className={css.sep}> | </span>
                        <span className={status === "unknown" ? undefined : css.metaHost}>{hubStore.hostName(instance.hostId)}</span>
                        {" / "}
                        {workspace?.label} · {instance.driver}
                      </div>
                    ) : (
                      <div className={css.meta}>
                        <span>{status}</span>
                        <span className={css.sep}>|</span>
                        <span className={css.metaHost}>{hubStore.hostName(instance.hostId)}</span>
                        <span>/ {workspace?.label}</span>
                        <span>· {instance.driver}</span>
                        <span className={css.sep}>|</span>
                        <span>{shortId(instance.id, 8)}</span>
                        {status === "working" ? <span>· {formatListTime(instance.updatedAt)}</span> : null}
                      </div>
                    )}
                    <div className={css.headline}>
                      <div className={`${css.name} ${status === "starting" || status === "exited" || status === "unknown" ? css.nameMute : ""}`}>
                        {title}
                      </div>
                      {badge ? <div className={css.badge}>{badge}</div> : null}
                      {exitLabel(instance) ? <div className={css.exit}>{exitLabel(instance)}</div> : null}
                    </div>
                    {status === "starting" ? (
                      <div className={css.cmd}>正在拉起 · lifecycle={instance.lifecycle}</div>
                    ) : status === "blocked" && pending?.request.kind === "approval" ? (
                      <div className={css.cmd}>{pending.request.description}</div>
                    ) : summary && status !== "blocked" ? (
                      <div className={css.cmd}>{summary}</div>
                    ) : status === "idle" ? (
                      <div className={css.cmd}>回合结束、进程仍在 · 可继续 send</div>
                    ) : status === "unknown" ? (
                      <div className={css.cmd}>connectivity={instance.connectivity} · 不推断成功或结束</div>
                    ) : null}
                  </span>
                  <div className={css.side}>
                    {tty ? (
                      <span className={css.tty} title={nativeShort(instance)}>
                        终端
                      </span>
                    ) : null}
                    {status === "blocked" ? <span className={css.unread} /> : null}
                    <span className={css.time}>{status === "unknown" ? "—" : formatListTime(instance.updatedAt)}</span>
                  </div>
                </Link>
              );
            })}
          </section>
        );
      })}
    </div>
  );
}
