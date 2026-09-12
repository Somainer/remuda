import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { hubStore, useHub } from "../lib/store";
import { composing, useWorkbenchViewport } from "../lib/viewport";
import { readNewSessionPrefs, rememberNewSessionSuccess, sortRecent } from "../lib/prefs";
import {
  DELEGATION_OPTIONS,
  PERMISSION_OPTIONS,
  PTY_YOLO_FLAGS,
  YOLO_ACK,
  YOLO_HINT,
  normalizeDelegation,
  normalizePermissionMode,
  providerProfileForDelegation,
  ptyYoloHint,
  type DelegationId,
} from "../lib/sessionOptions";
import { readDeviceSettings } from "../features/settings";
import {
  effortAt,
  effortCaps,
  effortTable,
  isEmberTier,
  mapEffort,
  type EffortKind,
  type EffortSelection,
} from "../features/session/effort";
import type { DriverKind } from "../types/nativeRef";
import type { Kind } from "../types/instance";
import { cliSummary, installedCli, isStaleOffline, sortHostsOnlineFirst, useHostViews } from "../features/hosts";
import { defaultGatewayProfile, fromHub, type ProviderProfile } from "../features/providers";
import { api } from "../lib/api";
import css from "./NewSessionPage.module.css";

type CreateKind = Exclude<Kind, "generic">;
type CwdMode = "existing" | "worktree";

const KINDS: { id: CreateKind; label: string }[] = [
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
  { id: "grok", label: "Grok" },
  { id: "agy", label: "agy" },
  { id: "terminal", label: "Terminal" },
];

export function NewSessionPage() {
  const hub = useHub();
  const hostViews = useHostViews(hub.hosts, hub.instances);
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const prefs = readNewSessionPrefs();
  const device = readDeviceSettings();
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const [prompt, setPrompt] = useState("");
  const [hostId, setHostId] = useState(params.get("host") ?? prefs.hostId);
  const [workspaceId, setWorkspaceId] = useState(params.get("workspace") ?? prefs.workspaceId);
  const [model, setModel] = useState(prefs.model || "passthrough/auto");
  const [permissionMode, setPermissionMode] = useState(
    normalizePermissionMode(prefs.permissionMode || device.permissionDefault),
  );
  const [yoloAck, setYoloAck] = useState(false);
  const [effort, setEffort] = useState<EffortSelection>(() => effortAt("claude", device.defaultEffortIndex));
  const [delegation, setDelegation] = useState<DelegationId>(normalizeDelegation(prefs.delegation));
  const [kind, setKind] = useState<CreateKind>("claude");
  const [wantTty, setWantTty] = useState(false);
  const [cwdMode, setCwdMode] = useState<CwdMode>("existing");
  const [cwdPath, setCwdPath] = useState("");
  const [worktreeName, setWorktreeName] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [settingsOverlayPath, setSettingsOverlayPath] = useState("");
  const [claudeConfigDir, setClaudeConfigDir] = useState("");
  const [maxBudgetUsd, setMaxBudgetUsd] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [gatewayProfiles, setGatewayProfiles] = useState<ProviderProfile[]>([]);

  useEffect(() => {
    promptRef.current?.focus();
  }, []);

  useEffect(() => {
    void api
      .providerList()
      .then((page) => setGatewayProfiles(page.items.map(fromHub).filter((p) => p.delegation === "gateway")))
      .catch(() => setGatewayProfiles([]));
  }, []);

  const defaultGateway = defaultGatewayProfile(gatewayProfiles);

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
  const existingCwd = cwdPath.trim() || workspace?.rootPath || "";
  const canStart = Boolean(
    hostId &&
      !offline &&
      !busy &&
      (cwdMode === "existing" ? existingCwd : worktreeName.trim()),
  );
  const hosts = pickerHosts;
  const workspaces = sortRecent(hostWorkspaces, prefs.recentWorkspaceIds);
  const hostView = hostViews.find((h) => h.id === hostId);
  const hostCli = cliSummary(hostView?.cli ?? host?.cli);
  const supportedKinds = installedCli(hostView?.cli ?? host?.cli).map((entry) => entry.kind);
  const kindEnabled = (id: CreateKind) =>
    id === "terminal" ? true : supportedKinds.length ? supportedKinds.includes(id) : id === "claude";
  const activeKind: CreateKind = kindEnabled(kind)
    ? kind
    : (KINDS.find((item) => kindEnabled(item.id))?.id ?? "claude");
  const plainTerminal = activeKind === "terminal";
  const driver: DriverKind = plainTerminal
    ? "shell-pty"
    : activeKind === "claude"
      ? mobile || !wantTty
        ? "claude-print"
        : "claude-pty"
      : "generic-pty";
  const sessionEffort = effort.kind === activeKind ? effort : mapEffort(effort, activeKind as EffortKind);
  const close = () => navigate("/sessions");

  return (
    <div className={css.overlay}>
      <button type="button" className={css.scrim} aria-label="关闭遮罩" onClick={close} />
      <form
        className={css.sheet}
        data-testid="new-session-sheet"
        onSubmit={(e) => {
          e.preventDefault();
          if (!canStart) return;
          setBusy(true);
          setError(null);
          void (async () => {
            let cwd = existingCwd;
            let worktree: string | undefined;
            if (cwdMode === "worktree") {
              const created = await hubStore.createWorktree({
                hostId,
                name: worktreeName.trim(),
                base: "main",
              });
              cwd = created.path;
              worktree = created.name;
            }
            const instance = await hubStore.create({
              hostId,
              workspaceId: (cwd || workspace?.id) as string,
              kind: activeKind,
              driver,
              model,
              providerProfileId: providerProfileForDelegation(delegation, defaultGateway?.id),
              permissionMode: activeKind === "claude" ? permissionMode : "bypassPermissions",
              delegation,
              prompt,
              cwd,
              worktree,
              settingsOverlayPath: settingsOverlayPath || undefined,
              claudeConfigDir: claudeConfigDir || undefined,
              maxBudgetUsd: maxBudgetUsd || undefined,
              name: name || worktree || (plainTerminal ? "terminal" : undefined),
              effortIndex: sessionEffort.index,
              effortName: sessionEffort.name,
            });
            rememberNewSessionSuccess({
              hostId,
              workspaceId: workspace?.id ?? cwd,
              model,
              permissionMode,
              driver,
              delegation,
              effortIndex: sessionEffort.index,
              effortName: sessionEffort.name,
            });
            navigate(`/s/${instance.id}`);
          })()
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
            <span className={css.label}>{plainTerminal ? "启动命令（可空，默认 shell）" : "提示词 · 第一焦点"}</span>
            <textarea
              ref={promptRef}
              className={css.prompt}
              data-testid="new-session-prompt"
              autoFocus
              placeholder={plainTerminal ? "empty = login shell in cwd/worktree" : undefined}
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
              <span className={css.label}>cwd / worktree</span>
              <div className={css.seg}>
                <button
                  type="button"
                  className={`${css.choice} ${cwdMode === "existing" ? css.choiceOn : ""}`}
                  data-testid="cwd-mode-existing"
                  onClick={() => setCwdMode("existing")}
                >
                  已有目录
                </button>
                <button
                  type="button"
                  className={`${css.choice} ${cwdMode === "worktree" ? css.choiceOn : ""}`}
                  data-testid="cwd-mode-worktree"
                  onClick={() => setCwdMode("worktree")}
                >
                  新 worktree from main
                </button>
              </div>
              {cwdMode === "existing" ? (
                <>
                  <div className={css.selectWrap}>
                    <select
                      className={css.select}
                      data-testid="new-session-workspace"
                      value={workspace?.id ?? ""}
                      onChange={(e) => {
                        setWorkspaceId(e.target.value);
                        const next = hostWorkspaces.find((w) => w.id === e.target.value);
                        if (next?.rootPath) setCwdPath(next.rootPath);
                      }}
                    >
                      {workspaces.map((w) => (
                        <option key={w.id} value={w.id}>
                          {w.label} · {w.rootPath}
                        </option>
                      ))}
                    </select>
                  </div>
                  <div className={css.selectWrap}>
                    <input
                      className={css.select}
                      data-testid="new-session-cwd"
                      placeholder="absolute path on the host"
                      value={cwdPath || workspace?.rootPath || ""}
                      onChange={(e) => setCwdPath(e.target.value)}
                    />
                  </div>
                </>
              ) : (
                <div className={css.selectWrap}>
                  <input
                    className={css.select}
                    data-testid="new-session-worktree-name"
                    placeholder="name (e.g. grok-pong)"
                    value={worktreeName}
                    onChange={(e) => setWorktreeName(e.target.value.toLowerCase())}
                  />
                </div>
              )}
              <span className={css.hint}>
                {cwdMode === "worktree"
                  ? "POST /v1/worktrees → git worktree add -b wt/<name>/… from main"
                  : existingCwd || "host directory"}
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
                    data-testid={`new-session-kind-${k.id}`}
                    disabled={!kindEnabled(k.id)}
                    onClick={() => {
                      if (!kindEnabled(k.id)) return;
                      setKind(k.id);
                      setEffort((prev) => mapEffort(prev, k.id as EffortKind));
                    }}
                  >
                    {k.label}
                  </button>
                ))}
              </div>
            </fieldset>
            {plainTerminal ? (
              <label className={css.field}>
                <span className={css.label}>driver</span>
                <span className={css.hint} data-testid="new-session-terminal-driver">
                  shell-pty · cwd/worktree 上的真实 PTY
                </span>
              </label>
            ) : (
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
            )}
          </div>
          <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className={css.label}>权限</legend>
            <div className={`${css.seg} ${css.permRow}`} data-testid="new-session-perm-row">
              {PERMISSION_OPTIONS.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.choice} ${css.permChoice} ${permissionMode === opt.id ? (opt.id === "bypassPermissions" ? css.choiceDust : css.choiceOn) : ""}`}
                  data-testid={`new-session-perm-${opt.id}`}
                  onClick={() => setPermissionMode(opt.id)}
                >
                  {opt.label}
                  <span className={css.choiceId}>{opt.id}</span>
                </button>
              ))}
            </div>
            {permissionMode === "bypassPermissions" && activeKind === "claude" ? (
              <div className={css.yolo} data-testid="new-session-yolo-hint">
                <div className={css.yoloHead}>
                  <span className={css.yoloDot} />
                  <span className={css.yoloTitle}>yolo · 该会话不再产生任何审批</span>
                  <label className={css.yoloAck}>
                    <input
                      type="checkbox"
                      data-testid="new-session-yolo-ack"
                      checked={yoloAck}
                      onChange={(e) => setYoloAck(e.target.checked)}
                    />
                    {YOLO_ACK}
                  </label>
                </div>
                <div className={css.yoloBody}>{YOLO_HINT}</div>
              </div>
            ) : null}
            {plainTerminal ? (
              <div className={css.yolo} data-testid="new-session-terminal-hint">
                <div className={css.yoloHead}>
                  <span className={css.yoloDot} />
                  <span className={css.yoloTitle}>driver shell-pty</span>
                </div>
                <div className={css.yoloBody}>plain terminal · 默认打开终端 tab · 键鼠走 raw PTY</div>
              </div>
            ) : activeKind === "codex" || activeKind === "grok" || activeKind === "agy" ? (
              <div className={css.yolo} data-testid="new-session-pty-hint">
                <div className={css.yoloHead}>
                  <span className={css.yoloDot} />
                  <span className={css.yoloTitle}>driver generic-pty</span>
                </div>
                <div className={css.yoloBody}>
                  {ptyYoloHint(activeKind)} · {PTY_YOLO_FLAGS[activeKind]}
                </div>
              </div>
            ) : null}
          </fieldset>
          {effortCaps(activeKind).effort ? (
            <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }} data-testid="new-session-effort">
              <legend className={css.label}>effort</legend>
              <div className={`${css.seg} ${css.effortRow}`}>
                {effortTable(activeKind).map((tier, index) => {
                  const on = sessionEffort.index === index;
                  const top = isEmberTier(activeKind, index);
                  return (
                    <button
                      key={tier.name}
                      type="button"
                      className={`${css.choice} ${css.effortChoice} ${on ? css.choiceOn : ""} ${top ? css.choiceEmber : ""}`}
                      data-testid={`new-session-effort-${tier.name}`}
                      data-ember={top ? "1" : "0"}
                      data-selected={on ? "1" : "0"}
                      onClick={() => setEffort(effortAt(activeKind as EffortKind, index))}
                    >
                      {tier.name}
                    </button>
                  );
                })}
                <span className={css.hint}>写进 InstanceSpec，会话内可再改</span>
              </div>
            </fieldset>
          ) : null}
          <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className={css.label}>Provider / 鉴权</legend>
            <div className={css.seg}>
              {DELEGATION_OPTIONS.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.choice} ${delegation === opt.id ? css.choiceOn : ""}`}
                  data-testid={`new-session-delegation-${opt.id}`}
                  onClick={() => {
                    setDelegation(opt.id);
                    if (opt.id === "gateway" && defaultGateway?.defaultModel) setModel(defaultGateway.defaultModel);
                  }}
                >
                  {opt.label}
                </button>
              ))}
            </div>
            {delegation === "gateway" ? (
              <span className={css.hint} data-testid="new-session-gateway-profile">
                {defaultGateway
                  ? `${defaultGateway.name} · ${defaultGateway.defaultModel || defaultGateway.models[0] || "model"}`
                  : "请先在 Provider 页配置网关"}
              </span>
            ) : null}
          </fieldset>
          <div className={css.advanced}>
            <button type="button" className={css.advancedToggle} data-testid="new-session-advanced" onClick={() => setAdvanced(!advanced)}>
              <span>{advanced ? "▾" : "▸"}</span>
              <span>高级 · 驱动</span>
              <span className={css.m3}>
                {plainTerminal ? "shell-pty" : activeKind === "claude" ? "claude-print / claude-pty" : "generic-pty"}
              </span>
            </button>
            {advanced ? (
              <div className={css.driverList}>
                {plainTerminal ? (
                  <p className={css.hint}>kind terminal · driver shell-pty · 空 prompt 打开 login shell。</p>
                ) : mobile ? (
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
                    <input className={css.select} data-testid="new-session-overlay" value={settingsOverlayPath} onChange={(e) => setSettingsOverlayPath(e.target.value)} />
                  </div>
                </label>
                <label className={css.field}>
                  <span className={css.label}>CLAUDE_CONFIG_DIR</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} data-testid="new-session-config-dir" value={claudeConfigDir} onChange={(e) => setClaudeConfigDir(e.target.value)} />
                  </div>
                </label>
                <label className={css.field}>
                  <span className={css.label}>max budget (USD)</span>
                  <div className={css.selectWrap}>
                    <input className={css.select} data-testid="new-session-budget" value={maxBudgetUsd} onChange={(e) => setMaxBudgetUsd(e.target.value)} />
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
