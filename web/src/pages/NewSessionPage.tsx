import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { hubStore, useHub } from "../lib/store";
import { composing, useWorkbenchViewport } from "../lib/viewport";
import { readNewSessionPrefs, rememberNewSessionSuccess, sortRecent } from "../lib/prefs";
import {
  DELEGATION_OPTIONS,
  PERMISSION_OPTIONS,
  YOLO_HINT,
  normalizeDelegation,
  normalizePermissionMode,
  providerProfileForDelegation,
  type DelegationId,
} from "../lib/sessionOptions";
import type { DriverKind } from "../types/nativeRef";
import type { Kind } from "../types/instance";
import { cliSummary, installedCli, isStaleOffline, sortHostsOnlineFirst } from "../features/hosts";
import css from "./NewSessionPage.module.css";

type CreateKind = Exclude<Kind, "generic">;

const KINDS: { id: CreateKind; label: string; enabled: boolean }[] = [
  { id: "claude", label: "Claude", enabled: true },
  { id: "codex", label: "Codex", enabled: false },
  { id: "grok", label: "Grok", enabled: false },
  { id: "agy", label: "agy", enabled: false },
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
  const [permissionMode, setPermissionMode] = useState(normalizePermissionMode(prefs.permissionMode));
  const [delegation, setDelegation] = useState<DelegationId>(normalizeDelegation(prefs.delegation));
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

  const pickerHosts = sortHostsOnlineFirst(
    hub.hosts.filter((h) => !isStaleOffline(h)),
    prefs.recentHostIds,
  );

  useEffect(() => {
    if (!pickerHosts.length) return;
    if (!hostId || !pickerHosts.some((h) => h.id === hostId)) {
      const preferred = pickerHosts.find((h) => h.id === prefs.hostId);
      setHostId(preferred?.id || pickerHosts[0].id);
    }
  }, [pickerHosts, hostId, prefs.hostId]);

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
  const hosts = pickerHosts;
  const workspaces = sortRecent(hostWorkspaces, prefs.recentWorkspaceIds);
  const hostCli = cliSummary(host?.cli);
  const supportedKinds = installedCli(host?.cli).map((entry) => entry.kind);
  const kindEnabled = (id: CreateKind) => (supportedKinds.length ? supportedKinds.includes(id) : id === "claude");
  const activeKind: CreateKind = kindEnabled(kind)
    ? kind
    : (KINDS.find((item) => kindEnabled(item.id))?.id ?? "claude");
  const close = () => navigate("/sessions");

  return (
    <div className={css.overlay}>
      <button type="button" className={css.scrim} aria-label="关闭遮罩" onClick={close} />
      <form
        className={css.sheet}
        data-testid="new-session-sheet"
        onSubmit={(e) => {
          e.preventDefault();
          if (!canStart || !workspace) return;
          setBusy(true);
          setError(null);
          void hubStore
            .create({
              hostId,
              workspaceId: workspace.id,
              kind: activeKind,
              driver,
              model,
              providerProfileId: providerProfileForDelegation(delegation),
              permissionMode,
              delegation,
              prompt,
              worktree,
              settingsOverlayPath: settingsOverlayPath || undefined,
              claudeConfigDir: claudeConfigDir || undefined,
              maxBudgetUsd: maxBudgetUsd || undefined,
              name: name || undefined,
            })
            .then((instance) => {
              rememberNewSessionSuccess({ hostId, workspaceId: workspace.id, model, permissionMode, driver, delegation });
              navigate(`/s/${instance.id}`);
            })
            .catch((err: unknown) => setError(err instanceof Error ? err.message : "create failed"))
            .finally(() => setBusy(false));
        }}
      >
        <div className={css.handle}>
          <div className={css.handleBar} />
        </div>
        <header className={css.head}>
          <h1 className={css.headTitle}>新建会话</h1>
          <button type="button" className={css.close} onClick={close} aria-label="关闭">
            ✕
          </button>
        </header>
        <div className={css.body}>
          <label className={css.field}>
            <span className={css.label}>提示词 · 第一焦点</span>
            <textarea
              ref={promptRef}
              className={css.prompt}
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
          <div className={css.pair}>
            <label className={css.field}>
              <span className={css.label}>主机</span>
              <div className={css.selectWrap}>
                <span className={`${css.hostDot} ${offline ? css.hostDotOff : ""}`} />
                <select
                  className={css.select}
                  data-testid="new-session-host"
                  value={hostId}
                  onChange={(e) => setHostId(e.target.value)}
                >
                  {hosts.map((h) => (
                    <option key={h.id} value={h.id}>
                      {h.label} · {h.state}
                      {cliSummary(h.cli) ? ` · ${cliSummary(h.cli)}` : ""}
                    </option>
                  ))}
                </select>
              </div>
              <span className={css.hint}>
                {host?.state === "online" || host?.online ? "在线" : host?.state}
                {hostCli ? ` · ${hostCli}` : ""}
              </span>
            </label>
            <label className={css.field}>
              <span className={css.label}>项目 · Workspace</span>
              <div className={css.selectWrap}>
                <select
                  className={css.select}
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
              </div>
              <span className={css.hint}>
                <span>{workspace?.rootPath}</span>
                <span className={css.worktree}>
                  <input type="checkbox" checked={worktree} onChange={(e) => setWorktree(e.target.checked)} />
                  新 worktree
                </span>
              </span>
            </label>
          </div>
          <div className={css.pair}>
            <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
              <legend className={css.label}>运行时</legend>
              <div className={css.seg}>
                {KINDS.map((k) => (
                  <button
                    key={k.id}
                    type="button"
                    className={`${css.choice} ${activeKind === k.id ? css.choiceOn : ""} ${kindEnabled(k.id) ? "" : css.choiceDisabled}`}
                    disabled={!kindEnabled(k.id)}
                    onClick={() => kindEnabled(k.id) && setKind(k.id)}
                  >
                    {k.label}
                  </button>
                ))}
              </div>
            </fieldset>
            <label className={css.field}>
              <span className={css.label}>模型</span>
              <div className={css.selectWrap}>
                <input
                  className={css.select}
                  data-testid="new-session-model"
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                />
              </div>
            </label>
          </div>
          <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className={css.label}>权限</legend>
            <div className={css.seg}>
              {PERMISSION_OPTIONS.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.choice} ${permissionMode === opt.id ? (opt.id === "bypassPermissions" ? css.choiceDust : css.choiceOn) : ""}`}
                  data-testid={`new-session-perm-${opt.id}`}
                  onClick={() => setPermissionMode(opt.id)}
                >
                  {opt.label}
                  <span className={css.choiceId}>{opt.id === "bypassPermissions" ? "bypassPermissions" : opt.id}</span>
                </button>
              ))}
            </div>
            {permissionMode === "bypassPermissions" ? (
              <div className={css.yolo} data-testid="new-session-yolo-hint">
                <div className={css.yoloHead}>
                  <span className={css.yoloDot} />
                  <span className={css.yoloTitle}>yolo · 该会话不再产生任何审批</span>
                </div>
                <div className={css.yoloBody}>{YOLO_HINT}</div>
              </div>
            ) : null}
          </fieldset>
          <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className={css.label}>Provider / 鉴权</legend>
            <div className={css.seg}>
              {DELEGATION_OPTIONS.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.choice} ${delegation === opt.id ? css.choiceOn : ""}`}
                  data-testid={`new-session-delegation-${opt.id}`}
                  onClick={() => setDelegation(opt.id)}
                >
                  {opt.label}
                </button>
              ))}
            </div>
          </fieldset>
          <div className={css.advanced}>
            <button type="button" className={css.advancedToggle} onClick={() => setAdvanced(!advanced)}>
              <span>{advanced ? "▾" : "▸"}</span>
              <span>高级 · 驱动</span>
              <span className={css.m3}>M3 才启用</span>
            </button>
            {advanced ? (
              <div className={css.driverList}>
                {mobile ? (
                  <p className={css.hint}>手机固定 structured print。</p>
                ) : (
                  <>
                    <button type="button" className={css.driverRow} onClick={() => setWantTty(false)}>
                      <span className={`${css.radio} ${wantTty ? "" : css.radioOn}`} />
                      <span>结构化 print</span>
                      <span className={css.driverId}>claude-print · structured-only · 默认</span>
                    </button>
                    <button type="button" className={`${css.driverRow} ${css.driverOff}`} disabled>
                      <span className={css.radio} />
                      <span>后台可唤醒</span>
                      <span className={css.driverIdOff}>claude-bg · 忽略 --session-id</span>
                    </button>
                    <button type="button" className={css.driverRow} onClick={() => setWantTty(true)}>
                      <span className={`${css.radio} ${wantTty ? css.radioOn : ""}`} />
                      <span>需要 /workflows 面板</span>
                      <span className={css.driverId}>claude-pty · 订阅登录 profile</span>
                    </button>
                  </>
                )}
                <label className={css.field}>
                  <span className={css.label}>settings overlay 路径</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} value={settingsOverlayPath} onChange={(e) => setSettingsOverlayPath(e.target.value)} />
                  </div>
                </label>
                <label className={css.field}>
                  <span className={css.label}>CLAUDE_CONFIG_DIR</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} value={claudeConfigDir} onChange={(e) => setClaudeConfigDir(e.target.value)} />
                  </div>
                </label>
                <label className={css.field}>
                  <span className={css.label}>max budget (USD)</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} value={maxBudgetUsd} onChange={(e) => setMaxBudgetUsd(e.target.value)} />
                  </div>
                </label>
                <label className={css.field}>
                  <span className={css.label}>name</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} value={name} onChange={(e) => setName(e.target.value)} />
                  </div>
                </label>
              </div>
            ) : null}
          </div>
          {error ? (
            <p data-testid="new-session-error" className={css.error}>
              {error}
            </p>
          ) : null}
          {offline ? <p className={css.hint}>主机离线，不能开始。</p> : null}
        </div>
        <footer className={css.foot}>
          <div className={css.footNote}>Provider {delegation === "gateway" ? "gateway" : "none"} · 默认全填上次成功值</div>
          <button type="button" className={css.cancel} onClick={close}>
            取消
          </button>
          <button type="submit" className={css.start} disabled={!canStart} data-testid="new-session-start">
            {busy ? "启动中" : "开始"}
          </button>
        </footer>
      </form>
    </div>
  );
}
