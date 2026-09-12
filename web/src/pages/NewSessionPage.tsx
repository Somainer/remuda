import { useMemo, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Button } from "../components/Button";
import { hubStore, useHub } from "../lib/store";
import { composing, useWorkbenchViewport } from "../lib/viewport";
import ui from "../styles/ui.module.css";

export function NewSessionPage() {
  const hub = useHub();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const defaultHost = params.get("host") ?? hub.hosts[0]?.id ?? "";
  const defaultWorkspace = params.get("workspace") ?? hub.workspaces[0]?.id ?? "";
  const [prompt, setPrompt] = useState("");
  const [hostId, setHostId] = useState(defaultHost);
  const [workspaceId, setWorkspaceId] = useState(defaultWorkspace);
  const [model, setModel] = useState("passthrough/auto");
  const [permissionMode, setPermissionMode] = useState("manual");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const host = hub.hosts.find((h) => h.id === hostId);
  const offline = host?.state !== "online" && host?.state !== "enrolled";
  const workspace = hub.workspaces.find((w) => w.id === workspaceId);

  const canStart = useMemo(() => Boolean(hostId && workspaceId && !offline && !busy), [hostId, workspaceId, offline, busy]);

  return (
    <form
      style={{ padding: 16, maxWidth: 560 }}
      onSubmit={(e) => {
        e.preventDefault();
        if (!canStart) return;
        setBusy(true);
        setError(null);
        void hubStore
          .create({
            hostId,
            workspaceId,
            kind: "claude",
            driver: "claude-print",
            model,
            providerProfileId: "astergate-default",
            permissionMode,
            prompt,
          })
          .then((instance) => navigate(`/s/${instance.id}`))
          .catch((err: unknown) => setError(err instanceof Error ? err.message : "create failed"))
          .finally(() => setBusy(false));
      }}
    >
      <h1 style={{ fontSize: 18, fontWeight: 600 }}>新建会话</h1>
      <label className={ui.field}>
        提示词
        <textarea
          className={ui.textarea}
          autoFocus
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={(e) => {
            if (composing(e)) return;
            if (!mobile && e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              (e.currentTarget.form as HTMLFormElement | null)?.requestSubmit();
            }
          }}
        />
      </label>
      <label className={ui.field} style={{ marginTop: 12 }}>
        主机
        <select className={ui.select} value={hostId} onChange={(e) => setHostId(e.target.value)}>
          {hub.hosts.map((h) => (
            <option key={h.id} value={h.id}>
              {h.label} · {h.state}
            </option>
          ))}
        </select>
      </label>
      <label className={ui.field} style={{ marginTop: 12 }}>
        项目（Workspace）
        <select className={ui.select} value={workspaceId} onChange={(e) => setWorkspaceId(e.target.value)}>
          {hub.workspaces.map((w) => (
            <option key={w.id} value={w.id}>
              {w.label} · {w.rootPath}
            </option>
          ))}
        </select>
      </label>
      <p className={ui.listMeta}>{workspace?.rootPath}</p>
      <label className={ui.field} style={{ marginTop: 12 }}>
        运行时
        <input className={ui.input} value="Claude" readOnly />
      </label>
      <label className={ui.field} style={{ marginTop: 12 }}>
        模型
        <input className={ui.select} value={model} onChange={(e) => setModel(e.target.value)} />
      </label>
      <fieldset style={{ border: 0, padding: 0, marginTop: 12 }}>
        <legend className={ui.listMeta}>权限</legend>
        <div className={ui.row}>
          {[
            { id: "manual", label: "询问" },
            { id: "acceptEdits", label: "可改文件" },
            { id: "dontAsk", label: "全自动" },
          ].map((opt) => (
            <button
              key={opt.id}
              type="button"
              className={`${ui.chip} ${permissionMode === opt.id ? ui.chipOn : ""}`}
              onClick={() => setPermissionMode(opt.id)}
            >
              {opt.label}
            </button>
          ))}
        </div>
      </fieldset>
      {error ? <p style={{ color: "var(--dust)" }}>{error}</p> : null}
      <div className={ui.row} style={{ marginTop: 16, justifyContent: "flex-end" }}>
        <Button onClick={() => navigate(-1)}>取消</Button>
        <Button variant="primary" type="submit" disabled={!canStart}>
          {busy ? "启动中" : "开始"}
        </Button>
      </div>
      <button type="submit" hidden />
    </form>
  );
}
