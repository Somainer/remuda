import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Button } from "../components/Button";
import { hubStore, useHub } from "../lib/store";
import { composing, useWorkbenchViewport } from "../lib/viewport";
import { readNewSessionPrefs, rememberNewSessionSuccess, sortRecent } from "../lib/prefs";
import ui from "../styles/ui.module.css";
import type { DriverKind } from "../types/nativeRef";
import type { Kind } from "../types/instance";

type CreateKind = Exclude<Kind, "generic">;

const KINDS: { id: CreateKind; label: string; enabled: boolean }[] = [
  { id: "claude", label: "Claude", enabled: true },
  { id: "codex", label: "Codex", enabled: false },
  { id: "grok", label: "Grok", enabled: false },
  { id: "agy", label: "agy", enabled: false },
];

const PERMS = [
  { id: "manual", label: "询问" },
  { id: "acceptEdits", label: "可改文件" },
  { id: "dontAsk", label: "全自动" },
];

export function NewSessionPage() {
  const hub = useHub();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const prefs = readNewSessionPrefs();
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const [prompt, setPrompt] = useState("");
  const [hostId, setHostId] = useState(params.get("host") ?? prefs.hostId);
  const [workspaceId, setWorkspaceId] = useState(params.get("workspace") ?? prefs.workspaceId);
  const [model, setModel] = useState(prefs.model || "passthrough/auto");
  const [permissionMode, setPermissionMode] = useState(prefs.permissionMode || "manual");
  const [kind, setKind] = useState<CreateKind>("claude");
  const [wantTty, setWantTty] = useState(false);
  const [worktree, setWorktree] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [settingsOverlayPath, setSettingsOverlayPath] = useState("");
  const [claudeConfigDir, setClaudeConfigDir] = useState("");
  const [maxBudgetUsd, setMaxBudgetUsd] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    promptRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!hostId && hub.hosts[0]) setHostId(prefs.hostId || hub.hosts[0].id);
  }, [hub.hosts, hostId, prefs.hostId]);

  useEffect(() => {
    const list = hub.workspaces.filter((w) => !hostId || w.hostId === hostId);
    if (!list.length) return;
    if (!workspaceId || !list.some((w) => w.id === workspaceId)) setWorkspaceId(list[0].id);
  }, [hub.workspaces, hostId, workspaceId]);

  const host = hub.hosts.find((h) => h.id === hostId);
  const offline = host?.state !== "online" && host?.state !== "enrolled";
  const hostWorkspaces = hub.workspaces.filter((w) => !hostId || w.hostId === hostId);
  const workspace = hostWorkspaces.find((w) => w.id === workspaceId) ?? hostWorkspaces[0];
  const driver: DriverKind = mobile || !wantTty ? "claude-print" : "claude-pty";
  const canStart = Boolean(hostId && workspace?.id && !offline && !busy);

  const hosts = sortRecent(hub.hosts, prefs.recentHostIds);
  const workspaces = sortRecent(hostWorkspaces, prefs.recentWorkspaceIds);

  return (
    <form
      className={ui.card}
      data-testid="new-session-sheet"
      style={{ margin: 16, maxWidth: 560, padding: 16 }}
      onSubmit={(e) => {
        e.preventDefault();
        if (!canStart || !workspace) return;
        setBusy(true);
        setError(null);
        void hubStore
          .create({
            hostId,
            workspaceId: workspace.id,
            kind,
            driver,
            model,
            providerProfileId: "astergate-default",
            permissionMode,
            prompt,
            worktree,
            settingsOverlayPath: settingsOverlayPath || undefined,
            claudeConfigDir: claudeConfigDir || undefined,
            maxBudgetUsd: maxBudgetUsd || undefined,
            name: name || undefined,
          })
          .then((instance) => {
            rememberNewSessionSuccess({ hostId, workspaceId: workspace.id, model, permissionMode, driver });
            navigate(`/s/${instance.id}`);
          })
          .catch((err: unknown) => setError(err instanceof Error ? err.message : "create failed"))
          .finally(() => setBusy(false));
      }}
    >
      <div className={ui.row} style={{ justifyContent: "space-between" }}>
        <h1 style={{ fontSize: 18, fontWeight: 600, margin: 0 }}>新建会话</h1>
        <Button variant="ghost" onClick={() => navigate(-1)} aria-label="关闭">
          ✕
        </Button>
      </div>
      <label className={ui.field} style={{ marginTop: 12 }}>
        提示词
        <textarea
          ref={promptRef}
          className={ui.textarea}
          data-testid="new-session-prompt"
          autoFocus
          value={prompt}
          onChange={(e) => setPrompt(e.target.value)}
          onKeyDown={(e) => {
            if (composing(e)) return;
            if (!mobile && e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.currentTarget.form?.requestSubmit();
            }
          }}
        />
      </label>
      <label className={ui.field} style={{ marginTop: 12 }}>
        主机
        <select
          className={`${ui.select} ${ui.touchSelect}`}
          data-testid="new-session-host"
          value={hostId}
          onChange={(e) => setHostId(e.target.value)}
        >
          {hosts.map((h) => (
            <option key={h.id} value={h.id}>
              {h.label} · {h.state}
            </option>
          ))}
        </select>
      </label>
      <p className={ui.listMeta}>{host?.state === "online" ? "在线" : host?.state} · claude 2.1.268</p>
      <label className={ui.field} style={{ marginTop: 12 }}>
        项目（Workspace）
        <select
          className={`${ui.select} ${ui.touchSelect}`}
          data-testid="new-session-workspace"
          value={workspace?.id ?? ""}
          onChange={(e) => setWorkspaceId(e.target.value)}
        >
          {workspaces.map((w) => (
            <option key={w.id} value={w.id}>
              {w.label} · {w.rootPath}
            </option>
          ))}
        </select>
      </label>
      <p className={ui.listMeta}>{workspace?.rootPath}</p>
      <label className={ui.row} style={{ marginTop: 8 }}>
        <input type="checkbox" checked={worktree} onChange={(e) => setWorktree(e.target.checked)} />
        新 worktree
      </label>
      <fieldset style={{ border: 0, padding: 0, marginTop: 12 }}>
        <legend className={ui.listMeta}>运行时</legend>
        <div className={ui.row}>
          {KINDS.map((k) => (
            <button
              key={k.id}
              type="button"
              className={`${ui.chip} ${kind === k.id ? ui.chipOn : ""}`}
              disabled={!k.enabled}
              onClick={() => k.enabled && setKind(k.id)}
            >
              {k.label}
              {k.id === "claude" ? " ●" : ""}
            </button>
          ))}
        </div>
      </fieldset>
      <label className={ui.field} style={{ marginTop: 12 }}>
        模型
        <input className={ui.select} data-testid="new-session-model" value={model} onChange={(e) => setModel(e.target.value)} />
      </label>
      <fieldset style={{ border: 0, padding: 0, marginTop: 12 }}>
        <legend className={ui.listMeta}>权限</legend>
        <div className={ui.row}>
          {PERMS.map((opt) => (
            <button
              key={opt.id}
              type="button"
              className={`${ui.chip} ${permissionMode === opt.id ? ui.chipOn : ""}`}
              onClick={() => setPermissionMode(opt.id)}
            >
              {opt.label}
              {opt.id === "manual" ? " ●" : ""}
            </button>
          ))}
        </div>
        {permissionMode === "dontAsk" ? (
          <p className={ui.listMeta} style={{ color: "var(--dust)" }}>
            全自动会跳过工具批准，仅限个人遥控；不要用于 bot 或写生产。
          </p>
        ) : null}
      </fieldset>
      {mobile ? (
        <p className={ui.listMeta}>手机固定 structured print。</p>
      ) : (
        <fieldset style={{ border: 0, padding: 0, marginTop: 12 }}>
          <legend className={ui.listMeta}>视图</legend>
          <div className={ui.row}>
            <button type="button" className={`${ui.chip} ${!wantTty ? ui.chipOn : ""}`} onClick={() => setWantTty(false)}>
              结构化 print
            </button>
            <button type="button" className={`${ui.chip} ${wantTty ? ui.chipOn : ""}`} onClick={() => setWantTty(true)}>
              需要 TUI → pty
            </button>
          </div>
        </fieldset>
      )}
      <button type="button" className={ui.chip} style={{ marginTop: 12 }} onClick={() => setAdvanced(!advanced)}>
        {advanced ? "收起高级" : "高级"}
      </button>
      {advanced ? (
        <div style={{ marginTop: 8 }}>
          <label className={ui.field}>
            settings overlay 路径
            <input className={ui.input} value={settingsOverlayPath} onChange={(e) => setSettingsOverlayPath(e.target.value)} />
          </label>
          <label className={ui.field} style={{ marginTop: 8 }}>
            CLAUDE_CONFIG_DIR
            <input className={ui.input} value={claudeConfigDir} onChange={(e) => setClaudeConfigDir(e.target.value)} />
          </label>
          <label className={ui.field} style={{ marginTop: 8 }}>
            max budget (USD)
            <input className={ui.input} value={maxBudgetUsd} onChange={(e) => setMaxBudgetUsd(e.target.value)} />
          </label>
          <label className={ui.field} style={{ marginTop: 8 }}>
            name
            <input className={ui.input} value={name} onChange={(e) => setName(e.target.value)} />
          </label>
        </div>
      ) : null}
      {error ? (
        <p data-testid="new-session-error" style={{ color: "var(--dust)" }}>
          {error}
        </p>
      ) : null}
      {offline ? <p className={ui.listMeta}>主机离线，不能开始。</p> : null}
      <div className={ui.row} style={{ marginTop: 16, justifyContent: "flex-end" }}>
        <Button onClick={() => navigate(-1)}>取消</Button>
        <Button variant="primary" type="submit" disabled={!canStart} data-testid="new-session-start">
          {busy ? "启动中" : "开始"}
        </Button>
      </div>
    </form>
  );
}
