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
  providerLaunchHint,
  providerProfileForDelegation,
  ptyYoloHint,
  type DelegationId,
} from "../lib/sessionOptions";
import { readDeviceSettings } from "../features/settings";
import { useNewSessionSpaceDefaults } from "../features/spaces/useNewSessionSpaceDefaults";
import {
  effortAt,
  effortCaps,
  effortWireName,
  mapEffort,
  normalizeClaudeName,
  type EffortKind,
  type EffortSelection,
} from "../features/session/effort";
import { EffortSlider } from "../features/session/EffortSlider";
import type { DriverKind } from "../types/nativeRef";
import type { Kind } from "../types/instance";
import { cliSummary, installedCli, isStaleOffline, sortHostsOnlineFirst, useHostViews } from "../features/hosts";
import {
  defaultGatewayProfile,
  enabledModels,
  fromHub,
  groupModels,
  resolveGatewayModel,
  type ProviderProfile,
} from "../features/providers";
import { api } from "../lib/api";
import { WorkspaceRegistration } from "../features/workspaces/WorkspaceRegistration";
import { workspaceCwd } from "../features/workspaces/path";
import css from "./NewSessionPage.module.css";

/** New Session writes the tier into the spec; the session can still change it later. */
const EFFORT_SPEC_HINT = "写进 InstanceSpec，会话内可再改";

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
  const prefs = { ...readNewSessionPrefs(), ...useNewSessionSpaceDefaults() };
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
  const [effort, setEffort] = useState<EffortSelection>(() => {
    // Remembered tier names predate the real --effort levels; map them by name.
    if (prefs.effortName) {
      const norm = normalizeClaudeName(prefs.effortName);
      return effortAt("claude", norm.index, norm.ultracode);
    }
    return effortAt("claude", device.defaultEffortIndex);
  });
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
      .providerList(hostId ? { hostId } : undefined)
      .then((page) => setGatewayProfiles(page.items.map(fromHub).filter((p) => p.delegation === "gateway")))
      .catch(() => setGatewayProfiles([]));
  }, [hostId]);

  const defaultGateway = defaultGatewayProfile(gatewayProfiles);
  // Only a gateway run is constrained to the profile's catalog; native and
  // direct sessions keep the free-text model box.
  const gatewayModels = delegation === "gateway" ? enabledModels(defaultGateway?.models ?? []) : [];
  // A gateway can publish hundreds of ids; group them the way the Provider
  // checklist does so the picker stays scannable.
  const gatewayGroups = groupModels(gatewayModels);
  // The catalog is the source of truth, so derive the choice during render
  // rather than repairing it in the delegation button's click handler: on a
  // slow runner the profile lands after the click, and a model the catalog
  // hides would otherwise linger as an extra option.
  const gatewayModel =
    delegation === "gateway" && defaultGateway
      ? resolveGatewayModel(defaultGateway.models, defaultGateway.defaultModel, model)
      : null;

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

  const host = hub.hosts.find((h) => h.id === hostId);
  const offline = host?.state !== "online" && host?.state !== "enrolled";
  const hostWorkspaces = hub.workspaces.filter((w) => w.hostId === hostId);
  const workspace = hostWorkspaces.find((w) => w.id === workspaceId) ?? hostWorkspaces[0];
  const existingCwd = workspace ? workspaceCwd(workspace.rootPath, cwdPath) : null;
  const canStart = Boolean(
    hostId &&
      !offline &&
      !busy &&
      workspace && (cwdMode === "existing" ? existingCwd : worktreeName.trim()),
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
  const claudeHint = providerLaunchHint({
    kind: activeKind,
    binding: hostView?.providerBinding ?? host?.providerBinding,
    cli: hostView?.cli ?? host?.cli,
    profiles: gatewayProfiles,
    delegation,
    explicitProfileId: providerProfileForDelegation(delegation, defaultGateway?.id),
  });
  const plainTerminal = activeKind === "terminal";
  const driver: DriverKind = plainTerminal
    ? "shell-pty"
    : activeKind === "claude"
      ? mobile || !wantTty
        ? "claude-print"
        : "claude-pty"
      : "generic-pty";
  const sessionEffort = effort.kind === activeKind ? effort : mapEffort(effort, activeKind as EffortKind);
  // Launch and remember what the picker shows, not a remembered id the
  // catalog has since stopped exposing.
  const launchModel = gatewayModel ?? model;
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
            let cwd = existingCwd ?? "";
            let worktree: string | undefined;
            if (cwdMode === "worktree") {
              const created = await hubStore.createWorktree({
                hostId,
                workspaceId: workspace?.id,
                name: worktreeName.trim(),
                base: "main",
              });
              cwd = created.path;
              worktree = created.name;
            }
            const instance = await hubStore.create({
              hostId,
              workspaceId: workspace?.id,
              kind: activeKind,
              driver,
              model: launchModel,
              providerProfileId: providerProfileForDelegation(delegation, defaultGateway?.id),
              permissionMode: activeKind === "claude" ? permissionMode : "bypassPermissions",
              delegation: delegation === "host" ? undefined : delegation,
              prompt,
              cwd,
              worktree,
              settingsOverlayPath: settingsOverlayPath || undefined,
              claudeConfigDir: claudeConfigDir || undefined,
              maxBudgetUsd: maxBudgetUsd || undefined,
              name: name || worktree || (plainTerminal ? "terminal" : undefined),
              effortIndex: sessionEffort.index,
              effortName: effortWireName(sessionEffort),
            });
            rememberNewSessionSuccess({
              hostId,
              workspaceId: workspace?.id ?? cwd,
              model: launchModel,
              permissionMode,
              driver,
              delegation,
              effortIndex: sessionEffort.index,
              effortName: effortWireName(sessionEffort),
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
                  onChange={(e) => { setHostId(e.target.value); setCwdPath(""); }}
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
            <div className={css.field}>
              <span className={css.label}>工作目录</span>
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
              <div className={css.selectWrap}>
                <select
                  className={css.select}
                  data-testid="new-session-workspace"
                  aria-label="已注册目录"
                  value={workspace?.id ?? ""}
                  onChange={(e) => {
                    setWorkspaceId(e.target.value);
                    setCwdPath("");
                  }}
                >
                  {!workspaces.length ? <option value="">请选择或添加目录</option> : null}
                  {workspaces.map((w) => (
                    <option key={w.id} value={w.id}>
                      {w.label} · {w.rootPath}
                    </option>
                  ))}
                </select>
              </div>
              <WorkspaceRegistration key={hostId} hostId={hostId} disabled={!hostId || offline || busy}
                onRegistered={(added) => { setWorkspaceId(added.id); setCwdPath(""); }} />
              {cwdMode === "existing" ? (
                <>
                  <label className={css.label} htmlFor="new-session-subpath">目录内子路径（可选）</label>
                  <div className={css.selectWrap}>
                    <input
                      id="new-session-subpath"
                      className={css.select}
                      data-testid="new-session-cwd"
                      placeholder="留空使用目录根路径，例如 src"
                      value={cwdPath}
                      disabled={!workspace}
                      onChange={(e) => setCwdPath(e.target.value)}
                    />
                  </div>
                  {workspace && existingCwd === null ? <p className={css.error} role="alert">请输入所选目录内的相对子路径，不能跳到目录外。</p> : null}
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
                  ? `从所选目录的 main 分支创建独立 worktree${workspace ? ` · ${workspace.rootPath}` : ""}`
                  : existingCwd || "先添加这台主机上的项目目录"}
              </span>
            </div>
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
                  {gatewayModels.length ? (
                    // A gateway profile publishes a catalog; offer only the
                    // models it exposes rather than a free-text box. Large
                    // catalogs are grouped by id prefix, the same buckets the
                    // Provider checklist uses.
                    <select
                      className={css.select}
                      data-testid="new-session-model"
                      value={gatewayModel ?? model}
                      onChange={(e) => setModel(e.target.value)}
                    >
                      {gatewayGroups.length > 1
                        ? gatewayGroups.map((group) => (
                            <optgroup key={group.key} label={group.key}>
                              {group.models.map((m) => (
                                <option key={m.id} value={m.id}>
                                  {m.label ? `${m.id} · ${m.label}` : m.id}
                                </option>
                              ))}
                            </optgroup>
                          ))
                        : gatewayModels.map((m) => (
                            <option key={m.id} value={m.id}>
                              {m.label ? `${m.id} · ${m.label}` : m.id}
                            </option>
                          ))}
                    </select>
                  ) : (
                    <input
                      className={css.select}
                      data-testid="new-session-model"
                      value={model}
                      onChange={(e) => setModel(e.target.value)}
                    />
                  )}
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
            <fieldset
              className={css.field}
              style={{ border: 0, padding: 0, margin: 0 }}
              data-testid="new-session-effort"
              data-harness={activeKind}
              data-effort={effortWireName(sessionEffort)}
              data-ultracode={sessionEffort.ultracode ? "1" : "0"}
            >
              {/*
               * Layout A: no card. The label row, the dotted pill spanning the
               * same column as the 权限 row, the five tick labels and the spec
               * helper all use the form's own tokens. Remounting on the
               * harness drops the drag draft, so the pill re-snaps onto the
               * new table instead of showing the stop the pointer left behind.
               */}
              <EffortSlider
                key={activeKind}
                kind={activeKind}
                index={sessionEffort.index}
                ultracode={sessionEffort.ultracode === true}
                variant="inline"
                idPrefix="new-session-effort"
                label="effort"
                footer={EFFORT_SPEC_HINT}
                onChange={(next) => setEffort(next)}
              />
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
                  onClick={() => setDelegation(opt.id)}
                >
                  {opt.label}
                </button>
              ))}
            </div>
            {delegation === "gateway" ? (
              <span className={css.hint} data-testid="new-session-gateway-profile">
                {defaultGateway
                  ? `${defaultGateway.name} · ${defaultGateway.defaultModel || gatewayModels[0]?.id || "model"}`
                  : "请先在 Provider 页配置网关"}
              </span>
            ) : null}
            {claudeHint ? (
              <span className={css.hint} data-testid="new-session-claude-auth-hint">
                {claudeHint}
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
          <div className={css.footNote}>Provider {delegation === "host" ? "跟随主机" : delegation} · 默认全填上次成功值</div>
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
