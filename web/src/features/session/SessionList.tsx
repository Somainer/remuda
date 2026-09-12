import { Link, useLocation } from "react-router-dom";
import type { Instance, UiStatus } from "../../types/instance";
import { knowledgeValue } from "../../types/command";
import { StateDot } from "../../components/StateDot";
import { projectStatus } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import ui from "../../styles/ui.module.css";

const GROUPS: { id: string; title: string; match: (s: UiStatus) => boolean }[] = [
  { id: "blocked", title: "待处理", match: (s) => s === "blocked" },
  { id: "working", title: "进行中", match: (s) => s === "working" || s === "starting" },
  { id: "recent", title: "最近", match: (s) => s === "idle" || s === "exited" || s === "unknown" },
];

export function SessionList({
  instances,
  query,
  statusFilter,
}: {
  instances: Instance[];
  query: string;
  statusFilter: string;
}) {
  const hub = useHub();
  const location = useLocation();
  const q = query.trim().toLowerCase();
  const filtered = instances.filter((instance) => {
    const status = projectStatus(instance);
    if (statusFilter && status !== statusFilter) return false;
    if (!q) return true;
    const title = hubStore.titleOf(instance.id).toLowerCase();
    const cwd = hubStore.workspaceOf(instance.workspaceId)?.rootPath.toLowerCase() ?? "";
    const native = instance.nativeRef.sessionId.state === "known" ? instance.nativeRef.sessionId.value.toLowerCase() : "";
    return title.includes(q) || cwd.includes(q) || native.includes(q) || instance.id.toLowerCase().includes(q);
  });

  return (
    <div>
      {GROUPS.map((group) => {
        const items = filtered.filter((i) => group.match(projectStatus(i)));
        if (!items.length) return null;
        return (
          <section key={group.id}>
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
                    : knowledgeValue(instance.activity) ?? instance.lifecycle;
              return (
                <Link key={instance.id} to={to} className={`${ui.listItem} ${active ? ui.listItemActive : ""}`}>
                  <StateDot status={status} />
                  <span>
                    <div>{hubStore.titleOf(instance.id)}</div>
                    <div className={ui.listMeta}>
                      {status} · {hubStore.hostName(instance.hostId)} · {hubStore.workspaceOf(instance.workspaceId)?.label} · {instance.driver}
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
