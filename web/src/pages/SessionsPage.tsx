import { Link, useSearchParams } from "react-router-dom";
import { useWorkbenchViewport } from "../lib/viewport";
import { useHub } from "../lib/store";
import { SessionList } from "../features/session/SessionList";
import { Button } from "../components/Button";
import ui from "../styles/ui.module.css";
import { useState } from "react";

export function SessionsPage() {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  const [params, setParams] = useSearchParams();
  const [q, setQ] = useState("");
  const status = params.get("status") ?? "";
  const host = params.get("host") ?? "";
  const workspace = params.get("workspace") ?? "";
  const kind = params.get("kind") ?? "";

  const instances = hub.instances.filter((i) => {
    if (host && i.hostId !== host) return false;
    if (workspace && i.workspaceId !== workspace) return false;
    if (kind && i.kind !== kind) return false;
    return true;
  });

  if (!mobile) {
    return (
      <div style={{ padding: 24, color: "var(--mute)" }}>
        {hub.hosts.length === 0 ? (
          <p>
            无主机。 <Link to="/hosts">添加主机</Link>
          </p>
        ) : hub.instances.length === 0 ? (
          <p>
            还没有会话。 <Link to="/sessions/new">新建会话</Link>
          </p>
        ) : (
          <p>选择一个会话</p>
        )}
      </div>
    );
  }

  return (
    <div>
      <header style={{ padding: 12, display: "flex", gap: 8 }}>
        <input className={ui.input} placeholder="搜索标题 / cwd / 原生 id" value={q} onChange={(e) => setQ(e.target.value)} />
        <Link to="/sessions/new">
          <Button variant="primary">新建</Button>
        </Link>
      </header>
      <div className={ui.row} style={{ padding: "0 12px 8px" }}>
        {["", "blocked", "working", "idle", "exited"].map((s) => (
          <button
            key={s || "all"}
            className={`${ui.chip} ${status === s ? ui.chipOn : ""}`}
            onClick={() => {
              const next = new URLSearchParams(params);
              if (s) next.set("status", s);
              else next.delete("status");
              setParams(next);
            }}
          >
            {s || "全部"}
          </button>
        ))}
      </div>
      <SessionList instances={instances} query={q} statusFilter={status} />
    </div>
  );
}
