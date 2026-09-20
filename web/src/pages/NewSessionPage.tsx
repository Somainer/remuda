import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useNavigationType, useSearchParams } from "react-router-dom";
import { hubStore, useHub } from "../lib/store";
import { composing, useWorkbenchViewport } from "../lib/viewport";
import { readNewSessionPrefs, rememberNewSessionSuccess, sortRecent } from "../lib/prefs";
import {
  DELEGATION_OPTIONS,
  PTY_YOLO_FLAGS,
  TUI_OPTIONS,
  TUI_LAUNCH_HINT,
  YOLO_ACK,
  YOLO_HINT,
  normalizeDelegation,
  providerLaunchHint,
  providerProfileForDelegation,
  ptyYoloHint,
  type DelegationId,
} from "../lib/sessionOptions";
import {
  codexSandboxTable,
  defaultPermissionForKind,
  launchPermissionTable,
  normalizePermissionMode,
  type PermissionOption,
} from "../features/session/permissions";
import { readDeviceSettings } from "../features/settings";
import { useNewSessionSpaceDefaults } from "../features/spaces/useNewSessionSpaceDefaults";
import { spaceKey, spaceStore } from "../features/spaces/store";
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
import { Sheet } from "../components/Sheet";
import { HubHttpError } from "../lib/httpError";
import {
  clearNewSessionDraft,
  draftAuthSubject,
  loadNewSessionDraft,
  saveNewSessionDraft,
} from "../lib/newSessionDraft";
import { COMMAND_STATUS_LABEL } from "../lib/commandStatus";
import { notify } from "../lib/notify";
import type { DriverKind } from "../types/nativeRef";
import type { Kind, TuiMode } from "../types/instance";
import { cliSummary, isStaleOffline, sortHostsOnlineFirst, supportedHarnessKinds, useHostViews } from "../features/hosts";
import {
  defaultDriver,
  DRIVER_LABELS,
  launchPreview,
  legacyDrivers,
  shellPtyAllowed,
  type AgentKindId,
  type HostMatrix,
} from "../lib/driverMatrix";
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

/**
 * Layout A keeps a muted helper under the effort field (composer-slider-4).
 * Copy is user vocabulary — the InstanceSpec it writes is an implementation
 * detail and lives with the other carrier details under 高级设置.
 */
const EFFORT_HELP = "会话开始后仍可在会话内调整";

/** P0-3 vocabulary (batch C1 commandStatus): the create left the page but its result is unknown. */
const STATUS_UNCERTAIN = COMMAND_STATUS_LABEL.unconfirmed;

type CreateKind = Exclude<Kind, "generic">;
type CwdMode = "existing" | "worktree";
type SubmitPhase = "idle" | "creating" | "unknown";

const KINDS: { id: CreateKind; label: string }[] = [
  { id: "claude", label: "Claude" },
  { id: "codex", label: "Codex" },
  { id: "grok", label: "Grok" },
  { id: "agy", label: "agy" },
  { id: "terminal", label: "终端" },
];

/** User-facing names for the model-source choice; raw ids stay in test ids. */
const DELEGATION_LABELS: Record<DelegationId, string> = {
  host: "跟随主机",
  none: "原生登录态",
  gateway: "网关",
};

function hostStateText(state: string | undefined, online?: boolean): string {
  return state === "online" || state === "enrolled" || online ? "在线" : "离线";
}

/** Client-side request identity for one create attempt (P0-2.5). */
function newClientRequestId(): string {
  const cryptoObj = globalThis.crypto as Crypto | undefined;
  if (cryptoObj && typeof cryptoObj.randomUUID === "function") return `creq_${cryptoObj.randomUUID()}`;
  const rand =
    typeof globalThis.crypto?.getRandomValues === "function"
      ? Array.from(globalThis.crypto.getRandomValues(new Uint8Array(8)), (b) => b.toString(16).padStart(2, "0")).join("")
      : Math.random().toString(16).slice(2, 18);
  return `creq_${Date.now().toString(16)}-${rand}`;
}

/**
 * A refusal the user can fix (or safely retry) on the form: every 4xx except
 * 408, plus 503 NODE_BUSY. That 503 is admitted here because the Hub refuses
 * the call *before* a frame reaches the Node — no session can have been
 * created, so 开始 again is safe. Anything else (aborted connection, gateway
 * timeout, other 5xx) leaves the server-side outcome unknown, and the page
 * must not offer a second create.
 */
function isDefiniteFailure(err: unknown): err is HubHttpError {
  if (!(err instanceof HubHttpError)) return false;
  if (err.status === 408) return false;
  if (err.status >= 400 && err.status < 500) return true;
  return err.status === 503 && err.code === "NODE_BUSY";
}

/** Inline failure text: machine code plus the Hub's human message. */
function formatCreateError(err: HubHttpError): string {
  return `${err.code} · ${err.message}`;
}

export function NewSessionPage() {
  const hub = useHub();
  const hostViews = useHostViews(hub.hosts, hub.instances);
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const { mobile } = useWorkbenchViewport();
  const prefs = { ...readNewSessionPrefs(), ...useNewSessionSpaceDefaults() };
  const device = readDeviceSettings();
  // The Hub-issued device id is the reliable draft-isolation subject. With no
  // session (logged out / shared machine) drafts stay in tab memory only.
  const authSubject = draftAuthSubject(hub.session);
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const [prompt, setPrompt] = useState("");
  const [hostId, setHostId] = useState(params.get("host") ?? prefs.hostId);
  const [workspaceId, setWorkspaceId] = useState(params.get("workspace") ?? prefs.workspaceId);
  const [model, setModel] = useState(prefs.model || "passthrough/auto");
  const [permissionMode, setPermissionMode] = useState(
    normalizePermissionMode("claude", prefs.permissionMode || device.permissionDefault),
  );
  // Codex's second axis: the sandbox mode paired with the approval policy.
  const [codexSandbox, setCodexSandbox] = useState<string>("workspace-write");
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
  const [driverOverride, setDriverOverride] = useState<DriverKind | null>(null);
  const [cwdMode, setCwdMode] = useState<CwdMode>("existing");
  const [cwdPath, setCwdPath] = useState("");
  const [worktreeName, setWorktreeName] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [settingsOverlayPath, setSettingsOverlayPath] = useState("");
  const [claudeConfigDir, setClaudeConfigDir] = useState("");
  const [maxBudgetUsd, setMaxBudgetUsd] = useState("");
  // Args are kept as the raw string the user typed and split on submit, so
  // the field stays editable mid-word. `launchArgTokens` is what actually
  // goes on the wire, and is shown as chips so the argv-not-shell rule is
  // visible rather than something you have to know.
  const [launchArgs, setLaunchArgs] = useState(prefs.launchArgs ?? "");
  const [binaryPath, setBinaryPath] = useState("");
  const [tui, setTui] = useState<TuiMode | undefined>(undefined);
  const [name, setName] = useState("");
  const [phase, setPhase] = useState<SubmitPhase>("idle");
  // `error` is a definite, fixable refusal; `uncertain` means the create may
  // have reached the host and must never be automatically retried.
  const [error, setError] = useState<string | null>(null);
  const errorRef = useRef<HTMLParagraphElement | null>(null);
  // The alert sits at the end of the scrolling form; on a 390px sheet the
  // keyboard-focused prompt sits at the top, so bring the failure into view
  // (and screen-reader range) the moment it appears.
  useEffect(() => {
    // jsdom has no scrollIntoView; only call it where the browser provides it.
    if (error) errorRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [error]);
  const [statusChecked, setStatusChecked] = useState(false);
  const [clientRequestId, setClientRequestId] = useState<string | null>(null);
  const [gatewayProfiles, setGatewayProfiles] = useState<ProviderProfile[]>([]);
  // D-047 per-dispatch delivery override, shown next to the model source.
  // "" = no override (request > project > profile > direct waterfall);
  // "none"/"self"/<hostId> force the route for this one session.
  const [apiVia, setApiVia] = useState("");
  const [apiRouteMode, setApiRouteMode] = useState<"auto" | "hub-relay" | "direct-net">("auto");

  // Where the sheet was opened from. It has an origin when the browser history
  // already has an entry in this tab (real browser) or when the router
  // transitioned here via PUSH (an in-app link/button). Escape/取消 then go
  // back to that session/list; a fresh deep link falls back to this space's
  // list.
  const navigationType = useNavigationType();
  const canReturnRef = useRef<boolean>(
    navigationType === "PUSH" ||
      (typeof window !== "undefined" &&
        typeof window.history.state?.idx === "number" &&
        window.history.state.idx > 0),
  );
  // Guards the submit async path against a second requestSubmit between renders.
  const phaseRef = useRef<SubmitPhase>("idle");
  phaseRef.current = phase;

  const close = () => {
    if (canReturnRef.current) {
      navigate(-1);
      return;
    }
    // Deep link with no origin: land on this space's list when we know it.
    if (hostId && workspaceId) spaceStore.selectSpace(spaceKey(hostId, workspaceId));
    navigate("/sessions");
  };

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
  const launchArgTokens = launchArgs.split(/\s+/).filter(Boolean);
  // The host default is shown as a placeholder rather than written into the
  // field: typing over it would make a session value out of something the
  // operator set once, and the two are merged differently by the Hub.
  const hostArgsDefault = host?.defaultLaunchArgs?.join(" ") ?? "";
  const hostBinaryDefault =
    host?.claudeBinaryPath ??
    host?.cli?.find((entry) => entry.kind === "claude")?.path ??
    "";
  const offline = host?.state !== "online" && host?.state !== "enrolled";
  const hostWorkspaces = hub.workspaces.filter((w) => w.hostId === hostId);
  const workspace = hostWorkspaces.find((w) => w.id === workspaceId) ?? hostWorkspaces[0];
  const existingCwd = workspace ? workspaceCwd(workspace.rootPath, cwdPath) : null;
  const canStart = Boolean(
    hostId &&
      !offline &&
      phase === "idle" &&
      workspace && (cwdMode === "existing" ? existingCwd : worktreeName.trim()),
  );
  const hosts = pickerHosts;
  const workspaces = sortRecent(hostWorkspaces, prefs.recentWorkspaceIds);
  const hostView = hostViews.find((h) => h.id === hostId);
  const hostCli = cliSummary(hostView?.cli ?? host?.cli);
  // Never the capability row: it is a host fact, not a harness. If it entered
  // this list, `supportedKinds.length` would go non-zero on a host that has
  // the vendor client but no agent CLI on PATH, suppressing the `claude`
  // fallback below and leaving every kind disabled.
  const supportedKinds = supportedHarnessKinds(hostView?.cli ?? host?.cli);
  const kindEnabled = (id: CreateKind) =>
    id === "terminal" ? true : supportedKinds.length ? supportedKinds.includes(id) : id === "claude";
  const activeKind: CreateKind = kindEnabled(kind)
    ? kind
    : (KINDS.find((item) => kindEnabled(item.id))?.id ?? "claude");

  // The launch table for the active harness; terminal has none.
  const permissionTable: PermissionOption[] =
    activeKind === "terminal" ? [] : launchPermissionTable(activeKind);
  // The dangerous/yolo row varies per harness and gates the ack checkbox.
  const dangerPermission = permissionTable.find((option) => option.danger);
  const yoloModeActive = Boolean(dangerPermission && permissionMode === dangerPermission.id);

  // Switching harness must never carry a foreign mode id into the create.
  useEffect(() => {
    if (activeKind === "terminal") return;
    if (!launchPermissionTable(activeKind).some((option) => option.id === permissionMode)) {
      setPermissionMode(defaultPermissionForKind(activeKind));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeKind]);
  const claudeHint = providerLaunchHint({
    kind: activeKind,
    binding: hostView?.providerBinding ?? host?.providerBinding,
    cli: hostView?.cli ?? host?.cli,
    profiles: gatewayProfiles,
    delegation,
    explicitProfileId: providerProfileForDelegation(delegation, defaultGateway?.id),
  });
  const plainTerminal = activeKind === "terminal";
  // D-028 §5.1: the matrix comes from the Node-reported driver inventory on
  // the host, never from a hardcoded driver table.
  const hostMatrix: HostMatrix = useMemo(
    () => ({
      cli: hostView?.cli ?? host?.cli,
      capabilities: host?.capabilities ?? null,
    }),
    [hostView?.cli, host?.cli, host?.capabilities],
  );
  const legacy = plainTerminal ? [] : legacyDrivers(activeKind as AgentKindId);
  const driver: DriverKind = plainTerminal
    ? "shell-pty"
    : driverOverride && legacy.includes(driverOverride)
      ? driverOverride
      : defaultDriver(hostMatrix, activeKind as AgentKindId);
  const nativeDefault = plainTerminal
    ? "shell-pty"
    : defaultDriver(hostMatrix, activeKind as AgentKindId);
  const driverChoices: { id: DriverKind; allowed: boolean }[] = plainTerminal
    ? [{ id: "shell-pty", allowed: true }]
    : [
        { id: "shell-pty", allowed: shellPtyAllowed(hostMatrix, activeKind as AgentKindId) },
        ...legacy.map((id) => ({ id, allowed: true })),
      ];
  const sessionEffort = effort.kind === activeKind ? effort : mapEffort(effort, activeKind as EffortKind);
  // Read-only preview of the launch the Node will prefill into the PTY. The
  // materialized recipe is Node-side (flags whitelist); until the Hub exposes
  // it, show the honest kind + flags summary (D-028 §5.1).
  const preview = plainTerminal
    ? launchPreview({ kind: "terminal" })
    : launchPreview({
        kind: activeKind as AgentKindId,
        effortName: effortWireName(sessionEffort),
        yolo: yoloModeActive,
      });
  // Launch and remember what the picker shows, not a remembered id the
  // catalog has since stopped exposing.
  const launchModel = gatewayModel ?? model;

  const contextKey = workspace?.id ? `${authSubject ?? ""}|${hostId}|${workspace.id}` : "";
  // Restoring is one effect behind the context switch; persistence waits for
  // the restore render so the mount-time empty state can't overwrite the
  // stored draft before its text is applied.
  const [restoredKey, setRestoredKey] = useState("");
  // Restore this context's draft once each time host/workspace/identity changes.
  // Options the draft never stores (auth, overlay paths, argv, executable)
  // simply keep their defaults.
  useEffect(() => {
    if (!hostId || !workspace?.id) return;
    const draft = loadNewSessionDraft(authSubject, hostId, workspace.id);
    if (draft) {
      if (draft.prompt) setPrompt(draft.prompt);
      if (draft.kind && KINDS.some((item) => item.id === draft.kind)) setKind(draft.kind as CreateKind);
      if (draft.model) setModel(draft.model);
      if (draft.permissionMode)
        setPermissionMode(normalizePermissionMode(activeKind, draft.permissionMode));
      if (draft.delegation) setDelegation(normalizeDelegation(draft.delegation));
      if (draft.cwdMode === "existing" || draft.cwdMode === "worktree") setCwdMode(draft.cwdMode);
      if (draft.cwdPath !== undefined) setCwdPath(draft.cwdPath);
      if (draft.worktreeName !== undefined) setWorktreeName(draft.worktreeName);
      if (draft.effortKind && draft.effortIndex !== undefined && KINDS.some((item) => item.id === draft.effortKind)) {
        setEffort(effortAt(draft.effortKind as EffortKind, draft.effortIndex, draft.effortUltracode === true));
      }
    } else {
      // Switching into a context that has no draft must not silently carry
      // the previous context's body into it (acceptance: 切换主机/目录不串用).
      // Non-sensitive options stay as form defaults; only the body resets.
      setPrompt("");
      setCwdPath("");
      setWorktreeName("");
      setCwdMode("existing");
    }
    setRestoredKey(contextKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [contextKey, authSubject, hostId, workspace?.id]);

  // Continuously persist the body plus non-sensitive options for this
  // context — only after that context's restore has run. Escape/取消 close
  // over a restorable draft; only an explicit 丢弃草稿 or a confirmed create
  // removes it.
  useEffect(() => {
    if (!hostId || !workspace?.id || restoredKey !== contextKey) return;
    saveNewSessionDraft(authSubject, hostId, workspace.id, {
      prompt,
      kind,
      model,
      permissionMode,
      delegation,
      cwdMode,
      cwdPath,
      worktreeName,
      effortKind: effort.kind,
      effortIndex: effort.index,
      effortUltracode: effort.ultracode === true,
    });
  }, [
    restoredKey,
    contextKey,
    authSubject,
    hostId,
    workspace?.id,
    prompt,
    kind,
    model,
    permissionMode,
    delegation,
    cwdMode,
    cwdPath,
    worktreeName,
    effort,
  ]);

  const draftHasBody = Boolean(prompt.trim() || cwdPath.trim() || worktreeName.trim());

  const discardDraftAndClose = () => {
    if (hostId && workspace?.id) clearNewSessionDraft(authSubject, hostId, workspace.id);
    setPrompt("");
    setCwdPath("");
    setWorktreeName("");
    close();
  };

  const refreshStatusOnly = async () => {
    // Read-only reconciliation. Without a server-bound command id we do not
    // guess which instance is ours and never navigate or create again — the
    // list is where the user confirms whether the session appeared.
    await hubStore.refresh().catch(() => undefined);
    setStatusChecked(true);
  };

  return (
    <Sheet
      open
      onClose={close}
      variant={mobile ? "sheet" : "popover"}
      labelledBy="new-session-title"
      initialFocusRef={promptRef}
      className={css.sheet}
      testId="new-session-sheet"
    >
      <form
        className={css.sheetForm}
        data-client-request={clientRequestId ?? ""}
        onSubmit={(e) => {
          e.preventDefault();
          if (!canStart || phaseRef.current !== "idle" || !workspace) return;
          const requestId = newClientRequestId();
          setClientRequestId(requestId);
          setPhase("creating");
          phaseRef.current = "creating";
          setError(null);
          setStatusChecked(false);
          void (async () => {
            try {
              let cwd = existingCwd ?? "";
              let worktree: string | undefined;
              if (cwdMode === "worktree") {
                const created = await hubStore.createWorktree({
                  hostId,
                  workspaceId: workspace.id,
                  name: worktreeName.trim(),
                  base: "main",
                });
                cwd = created.path;
                worktree = created.name;
              }
              const instance = await hubStore.create({
                hostId,
                workspaceId: workspace.id,
                kind: activeKind,
                driver,
                model: launchModel,
                providerProfileId: providerProfileForDelegation(delegation, defaultGateway?.id),
                permissionMode: activeKind === "terminal" ? "manual" : permissionMode,
                sandbox: activeKind === "codex" ? codexSandbox : undefined,
                delegation: delegation === "host" ? undefined : delegation,
                prompt,
                cwd,
                worktree,
                settingsOverlayPath: settingsOverlayPath || undefined,
                claudeConfigDir: claudeConfigDir || undefined,
                maxBudgetUsd: maxBudgetUsd || undefined,
                args: launchArgTokens.length ? launchArgTokens : undefined,
                binaryPath: binaryPath.trim() || undefined,
                tui: activeKind === "claude" ? tui : undefined,
                name: name || worktree || (plainTerminal ? "terminal" : undefined),
                effortIndex: sessionEffort.index,
                effortName: effortWireName(sessionEffort),
                ...(apiVia
                  ? { apiVia, apiRoute: apiVia === "none" ? undefined : apiRouteMode }
                  : {}),
              });
              // The create is confirmed: this context's draft is spent.
              clearNewSessionDraft(authSubject, hostId, workspace.id);
              rememberNewSessionSuccess({
                hostId,
                workspaceId: workspace.id ?? cwd,
                model: launchModel,
                permissionMode,
                driver,
                delegation,
                effortIndex: sessionEffort.index,
                effortName: effortWireName(sessionEffort),
                // Args are remembered; the executable deliberately is not. A
                // path silently restored into a later session is the kind of
                // thing you would not think to check before starting a run.
                launchArgs,
              });
              phaseRef.current = "idle";
              navigate(`/s/${instance.id}`);
            } catch (err) {
              if (isDefiniteFailure(err)) {
                // Definite refusal: keep every input (and its draft) and let
                // the user fix the form and submit once. The Hub's code and
                // message stay visible inline (c-mobilenew: an opaque INTERNAL
                // used to leave the phone with a dead 开始 button).
                setError(formatCreateError(err));
                setPhase("idle");
                phaseRef.current = "idle";
              } else {
                // The request may have been created on the host. We do not
                // know which instance is ours, so we never resend. The
                // occurrence keeps its own panel; the standing blocking
                // region (plan §2 notify contract) keeps the fact visible on
                // the list the user navigates to to verify.
                setPhase("unknown");
                phaseRef.current = "unknown";
                notify({
                  subject: "新建会话",
                  stage: STATUS_UNCERTAIN,
                  reason: `创建结果没有确认（请求 ${requestId}），可能已创建会话；已停止，不会自动再次创建。`,
                  severity: "blocking",
                  key: `new-session:${requestId}`,
                  diagnostic: {
                    hostId,
                    reasonCode: "create-ack-unknown",
                    statusKey: "unconfirmed",
                  },
                });
              }
            }
          })();
        }}
      >
        <div className={css.handle}>
          <div className={css.handleBar} />
        </div>
        <header className={css.head}>
          <h1 className={css.headTitle} id="new-session-title">
            新建会话
          </h1>
          <button type="button" className={css.close} onClick={close} aria-label="关闭" title="关闭（草稿保留，可恢复）">
            ✕
          </button>
        </header>
        <div className={css.body}>
          <label className={css.field}>
            <span className={css.label}>{plainTerminal ? "启动命令（可空，默认打开登录 shell）" : "要做什么"}</span>
            <textarea
              ref={promptRef}
              className={css.prompt}
              data-testid="new-session-prompt"
              placeholder={plainTerminal ? "留空则在工作目录打开登录 shell" : "描述这次要完成的事"}
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
                      {h.label} · {hostStateText(h.state, h.online)}
                      {cliSummary(h.cli) ? ` · ${cliSummary(h.cli)}` : ""}
                    </option>
                  ))}
                </select>
              </div>
              <span className={css.hint}>
                {offline ? "离线" : "在线"}
                {hostCli ? ` · 已安装 ${hostCli}` : ""}
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
                  新建 worktree
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
              <WorkspaceRegistration key={hostId} hostId={hostId} disabled={!hostId || offline || phase !== "idle"}
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
                    placeholder="名称，例如 feat-spill"
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
              <legend className={css.label}>执行 agent</legend>
              <div className={css.seg}>
                {KINDS.map((k) => (
                  <button
                    key={k.id}
                    type="button"
                    className={`${css.choice} ${activeKind === k.id ? css.choiceOn : ""} ${kindEnabled(k.id) ? "" : css.choiceDisabled}`}
                    data-testid={`new-session-kind-${k.id}`}
                    disabled={!kindEnabled(k.id) || phase !== "idle"}
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
              <div className={css.field}>
                <span className={css.label}>终端</span>
                {/* Terminal is itself the technical surface: its one carrier is
                    named here so terminal launches are never a hidden choice. */}
                <span className={css.hint} data-testid="new-session-terminal-driver">
                  shell-pty · 在工作目录上打开真实终端，默认进入终端视图，也可切换结构化记录。
                </span>
              </div>
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
            {activeKind !== "terminal" ? (
              <div className={`${css.seg} ${css.permRow}`} data-testid="new-session-perm-row" data-harness={activeKind}>
                {permissionTable.map((opt) => (
                  <button
                    key={opt.id}
                    type="button"
                    className={`${css.choice} ${css.permChoice} ${permissionMode === opt.id ? (opt.danger ? css.choiceDust : css.choiceOn) : ""}`}
                    data-testid={`new-session-perm-${opt.id}`}
                    data-danger={opt.danger ? "1" : undefined}
                    title={opt.description}
                    onClick={() => setPermissionMode(opt.id)}
                  >
                    <span>{opt.label}</span>
                    <span className={css.permNative}>{opt.native}</span>
                  </button>
                ))}
              </div>
            ) : null}
            {activeKind === "codex" ? (
              <div className={`${css.seg} ${css.permRow}`} data-testid="new-session-sandbox-row">
                {codexSandboxTable().map((opt) => (
                  <button
                    key={opt.id}
                    type="button"
                    className={`${css.choice} ${css.permChoice} ${codexSandbox === opt.native ? (opt.danger ? css.choiceDust : css.choiceOn) : ""}`}
                    data-testid={`new-session-sandbox-${opt.native}`}
                    data-danger={opt.danger ? "1" : undefined}
                    title={opt.description}
                    onClick={() => setCodexSandbox(opt.native)}
                  >
                    <span>{opt.label}</span>
                    <span className={css.permNative}>{opt.native}</span>
                  </button>
                ))}
              </div>
            ) : null}
            {yoloModeActive && activeKind === "claude" ? (
              <div className={css.yolo} data-testid="new-session-yolo-hint">
                <div className={css.yoloHead}>
                  <span className={css.yoloDot} />
                  <span className={css.yoloTitle}>绕过全部：该会话不再产生任何审批</span>
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
               * same column as the 权限 row, the tick labels and the helper
               * all use the form's own tokens. Remounting on the harness
               * drops the drag draft, so the pill re-snaps onto the new table
               * instead of showing the stop the pointer left behind.
               */}
              <EffortSlider
                key={activeKind}
                kind={activeKind}
                index={sessionEffort.index}
                ultracode={sessionEffort.ultracode === true}
                variant="inline"
                idPrefix="new-session-effort"
                label="effort"
                footer={EFFORT_HELP}
                onChange={(next) => setEffort(next)}
              />
            </fieldset>
          ) : null}
          <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className={css.label}>模型来源</legend>
            <div className={css.seg}>
              {DELEGATION_OPTIONS.map((opt) => (
                <button
                  key={opt.id}
                  type="button"
                  className={`${css.choice} ${delegation === opt.id ? css.choiceOn : ""}`}
                  data-testid={`new-session-delegation-${opt.id}`}
                  onClick={() => setDelegation(opt.id)}
                >
                  {DELEGATION_LABELS[opt.id]}
                </button>
              ))}
            </div>
            {delegation === "gateway" ? (
              <span className={css.hint} data-testid="new-session-gateway-profile">
                {defaultGateway
                  ? // The model that will actually launch, not the profile's
                    // default: `launchModel` is what the request carries, so a
                    // model typed or picked here has to be what this line says.
                    // Showing `defaultModel` made the line contradict the run.
                    `${defaultGateway.name} · ${launchModel || "model"}`
                  : "请先在 Provider 页配置网关"}
              </span>
            ) : null}
            {delegation === "gateway" ? (
              <span className={css.hint} data-testid="new-session-api-via">
                <label>
                  模型 API 出口{" "}
                  <select
                    data-testid="new-session-api-via-select"
                    value={apiVia}
                    onChange={(e) => setApiVia(e.target.value)}
                  >
                    <option value="">按 profile（默认）</option>
                    <option value="none">强制直连（none）</option>
                    <option value="self">经 Hub 主机（self）</option>
                    {hosts.map((host) => (
                      <option key={host.id} value={host.id}>
                        经 {host.label}
                      </option>
                    ))}
                  </select>
                </label>
                {apiVia && apiVia !== "none" ? (
                  <label style={{ marginLeft: 8 }}>
                    路由{" "}
                    <select
                      data-testid="new-session-api-route-select"
                      value={apiRouteMode}
                      onChange={(e) =>
                        setApiRouteMode(e.target.value as typeof apiRouteMode)
                      }
                    >
                      <option value="auto">自动</option>
                      <option value="hub-relay">Hub 中转</option>
                      <option value="direct-net">直连网络</option>
                    </select>
                  </label>
                ) : null}
                {apiVia && hosts.find((host) => host.id === apiVia)?.state !== "online"
                  && hosts.find((host) => host.id === apiVia)?.state !== "enrolled"
                  && apiVia !== "none" && apiVia !== "self" ? (
                  <span data-testid="new-session-api-via-offline" style={{ color: "var(--dust)" }}>
                    该主机当前离线，派发将被 Hub 拒绝（api-via-host-offline），不会改道
                  </span>
                ) : null}
              </span>
            ) : null}
            {claudeHint ? (
              <span className={css.hint} data-testid="new-session-claude-auth-hint">
                {claudeHint}
              </span>
            ) : null}
          </fieldset>
          <div className={css.advanced}>
            <button
              type="button"
              className={css.advancedToggle}
              data-testid="new-session-advanced"
              aria-expanded={advanced}
              onClick={() => setAdvanced(!advanced)}
            >
              <span aria-hidden="true">{advanced ? "▾" : "▸"}</span>
              <span>高级设置</span>
            </button>
            {advanced ? (
              <div className={css.driverList}>
                {plainTerminal ? (
                  <p className={css.hint}>
                    终端 kind 固定由 shell-pty 承载：在 cwd/worktree 上打开真实 PTY，空启动命令打开 login shell。
                  </p>
                ) : (
                  <p className={css.hint}>
                    承载方式（driver）{driver}：Node 按 {activeKind} 的启动模板过 flags 白名单后拼 argv，不接受原样透传。
                  </p>
                )}
                {!plainTerminal ? (
                  <fieldset className={css.field} style={{ border: 0, padding: 0, margin: 0 }} data-testid="new-session-driver-row">
                    <legend className={css.label}>承载方式 · launch prefill</legend>
                    <div className={css.driverChoices}>
                      {driverChoices.map((choice) => (
                        <button
                          key={choice.id}
                          type="button"
                          className={`${css.driverChoice} ${driver === choice.id ? css.driverChoiceOn : ""} ${choice.allowed ? "" : css.choiceDisabled}`}
                          data-testid={`new-session-driver-${choice.id}`}
                          data-default={choice.id === nativeDefault ? "1" : "0"}
                          disabled={!choice.allowed}
                          onClick={() => setDriverOverride(choice.id === nativeDefault ? null : choice.id)}
                        >
                          <span className={`${css.radio} ${driver === choice.id ? css.radioOn : ""}`} />
                          <span className={css.driverName}>{DRIVER_LABELS[choice.id]}</span>
                          {choice.id === nativeDefault ? <span className={css.driverDefault}>默认</span> : null}
                        </button>
                      ))}
                    </div>
                    <pre className={css.launchPreview} data-testid="new-session-launch-preview">
                      {preview}
                    </pre>
                    <span className={css.hint}>
                      {driver === "shell-pty"
                        ? "Remuda 在自持 PTY 里预填启动命令并回车 · 终端与结构化两个视图都可用"
                        : "旧承载方式 · 结构化能力以该承载实际上报为准"}
                      {!shellPtyAllowed(hostMatrix, activeKind as AgentKindId)
                        ? " · 该主机未上报 shell-pty 可用（以 Node 上报的能力清单为准）"
                        : ""}
                    </span>
                    {driver === "shell-pty" ? (
                      <div className={css.yolo} data-testid="new-session-pty-hint">
                        <div className={css.yoloHead}>
                          <span className={css.yoloDot} />
                          <span className={css.yoloTitle}>shell-pty · 原生终端</span>
                        </div>
                        <div className={css.yoloBody}>
                          launch shim + per-session overlay · {PTY_YOLO_FLAGS[activeKind as keyof typeof PTY_YOLO_FLAGS] ?? ""} 仅在绕过全部时追加
                        </div>
                      </div>
                    ) : activeKind === "codex" || activeKind === "grok" || activeKind === "agy" ? (
                      <div className={css.yolo} data-testid="new-session-pty-hint">
                        <div className={css.yoloHead}>
                          <span className={css.yoloDot} />
                          <span className={css.yoloTitle}>generic-pty</span>
                        </div>
                        <div className={css.yoloBody}>
                          {ptyYoloHint(activeKind)} · {PTY_YOLO_FLAGS[activeKind]}
                        </div>
                      </div>
                    ) : null}
                  </fieldset>
                ) : null}
                {activeKind === "claude" ? (
                  <label className={css.field}>
                    <span className={css.label}>终端渲染</span>
                    <div className={css.selectWrap}>
                      <select
                        className={css.select}
                        data-testid="new-session-tui"
                        disabled={phase !== "idle"}
                        value={tui ?? host?.defaultTui ?? "fullscreen"}
                        onChange={(e) => setTui(e.target.value as TuiMode)}
                        aria-describedby="new-session-tui-hint"
                      >
                        {TUI_OPTIONS.map((option) => <option key={option.id} value={option.id}>{option.label}</option>)}
                      </select>
                    </div>
                    <span className={css.hint} id="new-session-tui-hint" data-testid="new-session-tui-hint">{TUI_LAUNCH_HINT}</span>
                  </label>
                ) : null}
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
                  <span className={css.label}>特殊参数</span>
                  <div className={css.selectWrap}>
                    <input
                      className={css.select}
                      data-testid="new-session-args"
                      placeholder={hostArgsDefault || "--effort high --add-dir /srv"}
                      value={launchArgs}
                      onChange={(e) => setLaunchArgs(e.target.value)}
                    />
                  </div>
                </label>
                {launchArgTokens.length ? (
                  <p className={css.hint} data-testid="new-session-args-chips">
                    {launchArgTokens.map((token, index) => (
                      <code key={`${token}-${index}`} className={css.m3}>
                        {token}
                      </code>
                    ))}
                  </p>
                ) : null}
                <p className={css.hint}>
                  按空格切成 argv 数组，不是 shell 字符串——引号和 <code>|</code> 不会被解析。只接受白名单
                  flag，与模板重复或重复出现都会被拒。留空则用主机默认
                  {hostArgsDefault ? `（${hostArgsDefault}）` : "（无）"}。
                </p>
                <label className={css.field}>
                  <span className={css.label}>claude 可执行文件</span>
                  <div className={css.selectWrap}>
                    <input
                      className={css.select}
                      data-testid="new-session-binary"
                      placeholder={hostBinaryDefault || "/opt/claude/bin/claude"}
                      value={binaryPath}
                      onChange={(e) => setBinaryPath(e.target.value)}
                    />
                  </div>
                </label>
                <p className={css.hint}>
                  host 上的绝对路径。Node 校验并 pin（必须可执行、不能落在实例目录或工作区内、不能 group/other
                  可写），不通过就直接失败，不会悄悄回落到 PATH 上的 claude。
                </p>
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
            <p data-testid="new-session-error" className={css.error} role="alert" ref={errorRef}>
              {error}
            </p>
          ) : null}
          {phase === "unknown" ? (
            <div className={css.uncertain} data-testid="new-session-unknown" role="status">
              <p className={css.uncertainTitle}>
                {STATUS_UNCERTAIN}
                <span className={css.uncertainId} data-testid="new-session-client-request-id">
                  {clientRequestId}
                </span>
              </p>
              <p className={css.uncertainBody}>
                创建请求可能已经送达主机，但结果没有确认。为避免出现第二个会话，不会自动再次创建；请刷新后到会话列表确认。
              </p>
              <div className={css.uncertainActions}>
                <button type="button" className={css.checkButton} data-testid="new-session-check" onClick={refreshStatusOnly}>
                  刷新状态
                </button>
                <button type="button" className={css.cancel} onClick={close}>
                  返回列表
                </button>
              </div>
              {statusChecked ? (
                <p className={css.hint} data-testid="new-session-check-note">
                  已刷新。若会话已创建，它会出现在列表中；在此之前不要再次提交。
                </p>
              ) : null}
            </div>
          ) : null}
          {offline ? <p className={css.hint}>主机离线，不能开始。</p> : null}
        </div>
        <footer className={css.foot}>
          {draftHasBody ? (
            <button
              type="button"
              className={css.discard}
              data-testid="new-session-discard"
              onClick={discardDraftAndClose}
            >
              丢弃草稿
            </button>
          ) : (
            <span className={css.footNote}>默认选项取自上次成功创建 · 当前模型来源：{DELEGATION_LABELS[delegation]}</span>
          )}
          <button type="button" className={css.cancel} onClick={close}>
            取消
          </button>
          <button
            type="submit"
            className={css.start}
            disabled={!canStart}
            data-testid="new-session-start"
            data-phase={phase}
          >
            {phase === "creating" ? "启动中…" : phase === "unknown" ? STATUS_UNCERTAIN : "开始"}
          </button>
        </footer>
      </form>
    </Sheet>
  );
}
