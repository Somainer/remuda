import {
  Profiler,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type ComponentProps,
  type ProfilerOnRenderCallback,
  type ReactNode,
} from "react";
import { Navigate, useLocation, useNavigate, useParams } from "react-router-dom";
import { FileText, Info, ListCollapse, MessageSquarePlus, Rows3, ScrollText, Search } from "lucide-react";
import { ConnectionIndicator } from "../components/ConnectionIndicator";
import { EndedBar } from "../chrome/EndedBar";
import { SessionHeader, useWideDesktop } from "../chrome/SessionHeader";
import { SessionMoreMenu, type MoreMenuItem } from "../chrome/SessionMoreMenu";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { ElicitationCard } from "../features/approvals/ElicitationCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Composer } from "../features/session/Composer";
import { steerHeldControl } from "../features/composer/state";
import { LaunchedByMark } from "../features/session/LaunchedBy";
import { allModelPinMismatches } from "../features/session/modelEffective";
import { RunDetails } from "../features/session/RunDetails";
import { contextPercent } from "../features/session/effort";
import { ptyYoloChipLabel } from "../lib/sessionOptions";
import { Transcript, type TranscriptHandle } from "../features/session/Transcript";
import { LiveStatusStrip } from "../features/session/live/LiveStatusStrip";
import { useTurnDecision } from "../features/session/useTurnDecision";
import { SessionNotifications } from "../features/session/notifications/SessionNotifications";
import { TaskTrack } from "../features/session/TaskTrack";
import {
  AnnotationBadge,
  AnnotationPanel,
  useAnnotationsContext,
  useSessionTask,
} from "../features/tasks/AnnotationPanel";
import { composeWithAnnotations } from "../features/tasks/annotations";
import { RawEvents } from "../features/session/RawEvents";
import { assembleTranscript, collectTasks, compactTranscript } from "../features/session/assemble";
import { readDismissedWorkflows } from "../features/session/workflowDismiss";
import { canShowTerminal, hasStructuredSignal, isTtyLabFixtureId, resolveTtyLabInstance, TerminalView } from "../features/session/tty";
import { ScreenView } from "../features/session/ScreenView";
import { nativeShort, isGenericPty, isPromoted, projectStatus, uiMode, UI_STATUS_LABEL } from "../lib/status";
import { apiRouteClause, apiRouteKind, routeDownMessage } from "../lib/apiRoute";
import { projectCommandStatus } from "../lib/commandStatus";
import { endReason, NODE_EPOCH_CHANGED } from "../lib/endReason";
import { bindingChipText, transcriptBinding } from "../lib/transcriptBinding";
import type { ResumeMode } from "../lib/api";
import { hubStore, type HubState } from "../lib/store";
import { e2eSeamsEnabled } from "../lib/e2eSeams";
import { usePublishedElementHeight } from "../lib/usePublishedElementHeight";
import type { Id } from "../types/wire";
import { useWorkbenchViewport } from "../lib/viewport";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { readSessionView, writeSessionView, type SessionView } from "../lib/viewPref";
import { FilesView } from "../features/files/FilesView";
import session from "../chrome/sessionPage.module.css";
import annCss from "../features/tasks/annotation.module.css";
import { profilingEnabled, reportProbe } from "../lib/profileFlags";
import type { Observation } from "../types/generated";
import type { UsageRollup } from "../features/session/contextUsage";

declare global {
  interface Window {
    /** c-composerpop e2e seam; installed by SessionPage. */
    __usageLab?: {
      setRollup: (rollup: UsageRollup) => void;
    };
  }
}

/** Stable empty list so an unfollowed session does not re-project every render. */
const NO_EVENTS: Observation[] = [];

const onSessionCommit: ProfilerOnRenderCallback = (_id, _phase, actualDuration) => {
  reportProbe("commit:SessionPage", { actualDuration });
};

/**
 * Structural equality over plain wire data (objects, arrays, primitives).
 * The Hub's 2 s list refresh rebuilds every row even when nothing changed;
 * this is what lets the page tell a real change from a re-fetched copy.
 */
function sameValue(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const other = b as unknown[];
    return a.length === other.length && a.every((item, index) => sameValue(item, other[index]));
  }
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) return false;
  return keys.every((key) => Object.prototype.hasOwnProperty.call(right, key) && sameValue(left[key], right[key]));
}

/**
 * Everything the page body reads from the hub, narrowed to this one
 * instance. The body subscribes to this slice, not the whole snapshot: an
 * emit that touches another session, the host/workspace lists, or re-fetches
 * an unchanged row selects an equal slice and the body does not re-render.
 */
function selectSession(state: HubState, instanceId: string) {
  const instance = state.instances.find((i) => i.id === instanceId);
  const kind = instance?.kind;
  return {
    ready: state.ready,
    connection: state.connection,
    compact: state.compact,
    instance,
    events: state.events[instanceId],
    journalStatus: state.journalStatus[instanceId],
    earlierFloor: state.journalFloors[instanceId] ?? null,
    pending: state.interactions.filter((i) => i.instanceId === instanceId && i.state === "pending"),
    bubbles: state.bubbles.filter((b) => b.instanceId === instanceId && b.state !== "settled"),
    // c-steer: Remuda-held queue rows (Enter while busy / while a question is
    // pending). Posted in order by flushHeld when the wait ends.
    held: hubStore.heldBubbles(instanceId),
    title: hubStore.titleOf(instanceId),
    hostName: instance ? hubStore.hostName(instance.hostId) : "",
    permissionMode: hubStore.permissionModeOf(instanceId),
    launchPermissionMode: hubStore.launchPermissionModeOf(instanceId),
    permissionEffective: hubStore.permissionEffectiveOf(instanceId),
    permissionPending: hubStore.permissionPendingOf(instanceId),
    model: hubStore.modelOf(instanceId, kind),
    models: hubStore.modelListOf(instanceId),
    modelEffective: hubStore.modelEffectiveOf(instanceId),
    modelPending: hubStore.modelPendingOf(instanceId),
    modelCatalog: hubStore.modelCatalogOf(instanceId),
    effort: hubStore.effortOf(instanceId, kind),
    effortEffective: hubStore.effortEffectiveOf(instanceId),
    effortPending: hubStore.effortPendingOf(instanceId),
    usageRollup: hubStore.usageRollupOf(instanceId),
  };
}

type SessionSlice = ReturnType<typeof selectSession>;

/**
 * The decoder builds each list row's capability snapshot afresh and stamps
 * it with a new client-side object id (lib/api.ts decodeInstance →
 * lib/capabilities.ts printCapabilities `id("obj_")`), so two decodes of an
 * unchanged row differ only in `capabilities.id`. That id is a local handle,
 * not a Hub fact, and nothing reads it: compare the rest.
 */
function sameInstance(a: SessionSlice["instance"], b: SessionSlice["instance"]): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return sameValue(
    { ...a, capabilities: { ...a.capabilities, id: null } },
    { ...b, capabilities: { ...b.capabilities, id: null } },
  );
}

function sameSlice(a: SessionSlice, b: SessionSlice): boolean {
  const keys = Object.keys(a) as (keyof SessionSlice)[];
  // The journal window is append-only and replaced on change: identity is
  // the cheap, exact test for it.
  return keys.every((key) =>
    key === "events"
      ? a.events === b.events
      : key === "instance"
        ? sameInstance(a.instance, b.instance)
        : sameValue(a[key], b[key]),
  );
}

function useSessionSlice(instanceId: string): SessionSlice {
  const cache = useRef<{ instanceId: string; state: HubState; slice: SessionSlice } | null>(null);
  const getSnapshot = useCallback(() => {
    const state = hubStore.getSnapshot();
    const last = cache.current;
    if (last && last.instanceId === instanceId && last.state === state) return last.slice;
    const next = selectSession(state, instanceId);
    const slice = last && last.instanceId === instanceId && sameSlice(last.slice, next) ? last.slice : next;
    cache.current = { instanceId, state, slice };
    return slice;
  }, [instanceId]);
  return useSyncExternalStore(hubStore.subscribe, getSnapshot, getSnapshot);
}

/**
 * The header with its Space drawer. The workbench hook reads the whole
 * host/workspace/instance lists; it lives here, below the page body, so a
 * list refresh re-renders the header's space chip and not the page.
 */
function WorkbenchSessionHeader(props: Omit<ComponentProps<typeof SessionHeader>, "spaces">) {
  const workbench = useSpaceWorkbench();
  return (
    <SessionHeader
      {...props}
      spaces={{
        spaces: workbench.spaces,
        active: workbench.active,
        prefs: workbench.prefs,
        instanceId: workbench.instanceId,
        onSelect: workbench.select,
      }}
    />
  );
}

/** The EndedBar's 「开新会话」 link follows the active Space (same isolation). */
function WorkbenchEndedBar(props: Omit<ComponentProps<typeof EndedBar>, "newHref">) {
  const { newHref } = useSpaceWorkbench();
  return <EndedBar {...props} newHref={newHref} />;
}

/** RunDetails' own key: the controlled panel persists through the page. */
const RUN_DETAILS_KEY = "runtime.run-details.open";

function readRunDetailsOpen(): boolean {
  try {
    return localStorage.getItem(RUN_DETAILS_KEY) === "1";
  } catch {
    return false;
  }
}

function writeRunDetailsOpen(open: boolean): void {
  try {
    localStorage.setItem(RUN_DETAILS_KEY, open ? "1" : "0");
  } catch {
    /* storage unavailable: state just does not persist */
  }
}

type SessionPageProps = { view?: "auto" | "structured" | "tty" | "files" | "events" };

/**
 * The perf probe counts SessionPage commits (an idle page should commit zero
 * times a second). The Profiler is mounted only under `?profile=1`.
 */
export function SessionPage(props: SessionPageProps) {
  if (!profilingEnabled) return <SessionPageBody {...props} />;
  return (
    <Profiler id="SessionPage" onRender={onSessionCommit}>
      <SessionPageBody {...props} />
    </Profiler>
  );
}

function SessionPageBody({
  view = "auto",
}: SessionPageProps) {
  const { instanceId = "" } = useParams();
  const hub = useSessionSlice(instanceId);
  const annotationPanel = useAnnotationsContext();
  const navigate = useNavigate();
  const location = useLocation();
  const { mobile, offsetTop } = useWorkbenchViewport();
  // The page-body commit probe (`?profile=1`). The Profiler around the page
  // also counts descendant commits (the live strip's clock, the header's
  // space chip); this one counts only the body re-rendering.
  useLayoutEffect(() => {
    if (profilingEnabled) reportProbe("commit:SessionPageBody", {});
  });
  // ui-spec §2.2: 「文件」 stays on the desktop row only from 1024px up; below
  // that (and on compact) it is a ⋯ item.
  const wide = useWideDesktop();
  const [moreOpen, setMoreOpen] = useState(false);
  // D-040 (3) / D-053: run details is folded by default and its open state is
  // remembered on this device; the ⋯ item is its only trigger.
  const [runDetailsOpen, setRunDetailsOpen] = useState(readRunDetailsOpen);
  const toggleRunDetails = useCallback((open: boolean) => {
    setRunDetailsOpen(open);
    writeRunDetailsOpen(open);
  }, []);
  const closeRunDetails = useCallback(() => toggleRunDetails(false), [toggleRunDetails]);
  const transcriptRef = useRef<TranscriptHandle | null>(null);
  const openTranscriptSearch = useCallback(() => transcriptRef.current?.openSearch(), []);
  const collapseTranscript = useCallback(() => transcriptRef.current?.collapseAll(), []);
  const [sendingIds, setSendingIds] = useState<string[]>([]);
  const sending = sendingIds.includes(instanceId);
  const setSending = (value: boolean) => setSendingIds((ids) => value ? [...new Set([...ids, instanceId])] : ids.filter((id) => id !== instanceId));
  const [resuming, setResuming] = useState(false);
  // 已打断 receipt shared by the composer controls and the transcript held-row
  // 插队发送 button, so the gesture reads the same from either place. Reset on
  // session switch; the composer also clears it on the next turn / after 4 s.
  const [interrupted, setInterrupted] = useState(false);
  useEffect(() => setInterrupted(false), [instanceId]);
  const instance = hub.instance ?? resolveTtyLabInstance(instanceId);
  // t-annotations: a session of an archived task is a read-only preview — no
  // badge/entry point and anchor selection raises nothing.
  const sessionTask = useSessionTask(instanceId, (instance as { taskId?: string | null } | undefined)?.taskId);
  const annotationReadonly = sessionTask?.archivedAt != null;
  const showTerminal = instance ? canShowTerminal(instance) : false;
  const showStructured = instance ? hasStructuredSignal(instance) : false;
  const remembered = showTerminal || showStructured ? readSessionView(instanceId) : null;
  // Both projections exist on a native PTY session; open the conversation by
  // default when a structured signal exists, the raw terminal otherwise
  // (D-028 §1.0 — the switch always offers the other projection).
  const baseView: SessionView = remembered ?? (showStructured ? "structured" : "tty");
  const backTo = `/s/${instanceId}/${baseView}`;

  useEffect(() => {
    if (instanceId && !isTtyLabFixtureId(instanceId)) void hubStore.follow(instanceId);
  }, [instanceId]);

  // c-composerpop e2e seam: inject a Hub-computed usage rollup as if a poll
  // had delivered it (fake-node sessions never report usage). Installed ONLY
  // when the e2e seam marker is set (hub-auth addInitScript); a production
  // session never gets this handle.
  useEffect(() => {
    if (!e2eSeamsEnabled() || !instanceId || isTtyLabFixtureId(instanceId)) return;
    window.__usageLab = {
      setRollup: (rollup: UsageRollup) => hubStore.setUsageRollupForTest(instanceId, rollup),
    };
    return () => {
      delete window.__usageLab;
    };
  }, [instanceId]);

  useEffect(() => {
    if (instanceId && (view === "tty" || view === "structured")) writeSessionView(instanceId, view);
  }, [instanceId, view]);

  // The files route replaces the conversation inside the same scroll container.
  // Remember the conversation position when leaving it and restore it on return
  // so back from the full-screen files view lands where the reader was.
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const preFilesScroll = useRef(0);
  const currentScroller = () =>
    document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")
      ?? document.querySelector<HTMLElement>(".xterm-viewport")
      ?? bodyRef.current;
  const openFiles = () => {
    preFilesScroll.current = currentScroller()?.scrollTop ?? 0;
    navigate(`/s/${instanceId}/files`);
  };

  // Esc backs out of the full-screen files route. Armed in a layout effect so it is
  // live as soon as the route commits, and in the capture phase so it still fires
  // when focus sits on a control that swallows the bubble.
  useLayoutEffect(() => {
    if (view !== "files") return;
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      // An open composer popover eats the first Esc.
      if (document.querySelector("[data-testid$='-menu'], [data-testid$='-popover']")) return;
      navigate(backTo, { replace: true });
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [view, backTo, navigate]);

  // c-composerpop r4 item 1: publish the bottom chrome's measured height as a
  // global custom property so the notify stack can anchor ABOVE the lowest
  // interactive surface on /s/:id. Shell renders ShellNotify as a sibling of
  // <main>, so a value set on a SessionPage node would not inherit to the
  // stack; ride documentElement (its common ancestor). The structured views
  // render the session dock (composer control bar); the tty view renders no
  // dock, so TerminalView hands up its bottom chrome (local input dock +
  // phone key bar, + the byte-route note on desktop). Exactly one is mounted
  // at a time and SessionPage is the single writer — two writers would race
  // on the same property across the tty/structured switch. The hook clears
  // the value with no element so non-session routes never see a stale height.
  const [dockEl, setDockEl] = useState<HTMLDivElement | null>(null);
  const [ttyChromeEl, setTtyChromeEl] = useState<HTMLDivElement | null>(null);
  usePublishedElementHeight(dockEl ?? ttyChromeEl, "--session-dock-h");
  // Durable-lifecycle end state (a failed process is red; a Node restart is
  // toned — only a failed ending is ever painted red). It reads the durable
  // lifecycle only: the Hub marks an ended row disconnected (a Node restart
  // does exactly that), and projectStatus answers "unknown" for any
  // disconnected row before it looks at the lifecycle. An ended session must
  // keep its EndedBar and Resume, and must never mount a live Composer, while
  // its host is away.
  const ended = instance ? endReason(instance) : null;
  const status = ended ? "exited" : instance ? projectStatus(instance) : "unknown";
  const events = hub.events ?? NO_EVENTS;
  const pending = hub.pending;
  // The turn-end decision folds every channel (hook latch, screen, transcript
  // tail, pending interactions), not the hook latch alone — so a turn whose
  // Stop hook never lands still ends once the screen/pty says idle. It is the
  // one decision the strip and the composer share, which is what lets a held
  // prompt flush on the same boundary the clock stops on. The hook-freshness
  // judgement is time-driven; useTurnDecision re-projects on a clock only
  // while the turn is open and commits only when the answer changes, so an
  // idle page does not re-render every second.
  const turnDecision = useTurnDecision(events, instance?.nativeRef, pending.length > 0);
  // D-028 §6 composer phase. `starting` behaves like idle (one send box);
  // only a live working/blocked turn exposes steer/queue/interrupt. The
  // multi-channel turn decision outranks the instance projection for the
  // open/ended call (an ended → idle edge is the held-queue flush boundary);
  // `unknown` defers to the instance projection instead of collapsing to
  // either idle or blocked.
  let composerPhase: "idle" | "working" | "blocked" | "exited";
  if (status === "exited") {
    composerPhase = "exited";
  } else if (turnDecision.state === "ended") {
    composerPhase = "idle";
  } else if (turnDecision.state === "waiting") {
    composerPhase = "blocked";
  } else if (turnDecision.state === "working") {
    composerPhase = "working";
  } else {
    composerPhase =
      status === "blocked" ? "blocked" : status === "working" ? "working" : "idle";
  }
  const nodeRestarted = instance?.lastError === NODE_EPOCH_CHANGED;
  const resolvedView = view === "auto" ? baseView : view;
  // Terminal segments offer no annotations; archived-task sessions are a
  // read-only preview (plan task-model task 9 acceptance 3).
  const annotationAllowed =
    resolvedView !== "tty" && resolvedView !== "events" && !annotationReadonly;

  // D-047: the route this session actually got — the Node-echoed clause only,
  // never the requested value (D-035). And the §B.5 block when its proxy host
  // went away: an error, not a changed route.
  const routeClause = instance ? apiRouteClause(instance.apiRoute) : null;
  const routeKind = instance ? apiRouteKind(instance.apiRoute) : null;
  const routeDown = routeKind ? routeDownMessage(events) : null;

  // Restore the saved reading position when leaving the files route. The
  // transcript is virtualized, so retry over a couple of frames after its rows
  // mount; setting scrollTop drives its range via the existing scroll handler.
  useLayoutEffect(() => {
    if (resolvedView === "files") return;
    const apply = () => {
      const scroller =
        document.querySelector<HTMLElement>("[data-testid='transcript-scroller']")
          ?? document.querySelector<HTMLElement>(".xterm-viewport")
          ?? bodyRef.current;
      if (scroller) scroller.scrollTop = preFilesScroll.current;
    };
    apply();
    const first = requestAnimationFrame(apply);
    const second = requestAnimationFrame(() => requestAnimationFrame(apply));
    return () => {
      cancelAnimationFrame(first);
      cancelAnimationFrame(second);
    };
  }, [resolvedView]);
  const journalStatus = hub.journalStatus ?? "live";
  const bubbles = hub.bubbles;
  const heldBubbles = hub.held;
  // C2: the header label speaks the P0-3 vocabulary while an optimistic
  // bubble is in flight (null commandId + unknown state → 「状态待确认」,
  // never a fake success). With no pending bubble it shows the instance-level
  // status wording as before. This is the only commandStatus call on the
  // page.
  const pendingBubble = bubbles.at(-1) ?? null;
  const commandRow = pendingBubble
    ? projectCommandStatus({
        instance,
        interaction: pending[0] ? { interaction: pending[0] } : null,
        hasServerCommandId: pendingBubble.commandId !== null,
        localState: pendingBubble.state,
      })
    : null;
  const statusLabel = commandRow?.label ?? UI_STATUS_LABEL[status];
  const usageEvent = events.findLast((e) => e.kind === "usage");
  const usage = usageEvent?.kind === "usage" ? usageEvent.payload : undefined;
  const tasks = collectTasks(
    compactTranscript(assembleTranscript(events, bubbles), hub.compact, readDismissedWorkflows(instanceId)),
  );
  // The initial seed is loading only while there is nothing to render.
  // Offline restore deliberately has NO events (the seed/follow fail), but
  // the durable outbox restores bubbles that must be shown over a working
  // composer — never held behind this gate until the network returns.
  const snapshotLoading =
    Boolean(instance) &&
    hub.events === undefined &&
    bubbles.length === 0 &&
    !isTtyLabFixtureId(instanceId);

  if (!instance && hub.ready) {
    return <p style={{ padding: 16 }}>会话不存在</p>;
  }
  if (!instance) return <p style={{ padding: 16 }}>加载 snapshot…</p>;

  if (resolvedView === "tty" && !showTerminal) {
    hubStore.toast("该会话没有可 attach 的终端");
    return <Navigate to={`/s/${instanceId}/structured`} replace />;
  }

  const cost = usage && usage.cost.state === "known" ? `$${usage.cost.value.amount}` : "—";
  const startResume = async (mode: ResumeMode) => {
    setResuming(true);
    try {
      const resumedId = await hubStore.resume(instanceId as Id, mode);
      // A failed resume already surfaced the Hub's reason as a toast; staying
      // put keeps the old transcript readable.
      if (resumedId) navigate(`/s/${resumedId}/${mode === "terminal" ? "tty" : "structured"}`);
    } finally {
      setResuming(false);
    }
  };
  const canResume = instance.capabilities.capabilities.resume?.state === "supported";
  const endedBar = ended ? (
    <WorkbenchEndedBar
      reason={ended}
      nodeRestarted={nodeRestarted}
      heldCount={heldBubbles.length}
      canResume={canResume}
      resuming={resuming}
      onResume={(mode) => {
        void startResume(mode);
      }}
    />
  ) : null;
  const connLabel = journalStatus === "live" ? hub.connection : journalStatus;
  const title = hub.title;
  const structuredOnly = uiMode(instance) === "structured-only";
  const genericPty = isGenericPty(instance);
  const promoted = isPromoted(instance);
  const binding = promoted ? transcriptBinding(events) : null;
  const activity = instance.activity.state === "known" ? instance.activity.value : instance.activity.state;
  // A tty-lab fixture is not a hub row, so the slice has no host for it.
  const hostName = hub.instance ? hub.hostName : hubStore.hostName(instance.hostId);
  const nativeRefShort = nativeShort(instance);
  const showViewExtras = resolvedView === "structured" || resolvedView === "files" || resolvedView === "events";
  const toggleFiles = () => {
    if (resolvedView === "files") navigate(backTo);
    else openFiles();
  };
  const transcriptMounted = resolvedView === "structured" && !snapshotLoading && !genericPty;
  const filesInline = wide && !mobile && showViewExtras;

  // The 运行详情 fields as one array: the ⋯ item advertises exactly the
  // number of items the panel reveals (ui-spec §2.2), so the count is derived,
  // never hand-maintained. host/cost keep the desktop main row; on compact
  // (and coarse-pointer compact) the panel is their only home (D-049).
  // Provenance and promotion live here on every width (D-053).
  const diagnostics: ReactNode[] = [];
  if (mobile) diagnostics.push(<span key="host" data-testid="run-details-host">{hostName}</span>);
  diagnostics.push(
    <span key="driver" data-testid="session-driver">
      {promoted ? `${instance.driver} · promoted` : instance.driver}
    </span>,
    <span key="delegation" data-testid="session-delegation">{instance.delegation ?? "none"}</span>,
    <span key="provider" data-testid="session-provider">{instance.providerProfileId ?? "none"}</span>,
  );
  if (instance.providerSourceHint) {
    diagnostics.push(<span key="provider-source" data-testid="session-provider-source">{instance.providerSourceHint}</span>);
  }
  if (routeClause) {
    diagnostics.push(
      <span
        key="api-route"
        data-testid="session-api-route"
        data-mode={instance.apiRoute?.mode ?? "direct"}
        data-route={routeKind ?? "direct"}
        data-down={routeDown ? "1" : "0"}
      >
        {routeClause}
      </span>,
    );
  }
  diagnostics.push(
    <span key="lifecycle" data-testid="session-lifecycle">{instance.lifecycle}</span>,
    <span key="seq">seq {events.at(-1)?.seq ?? instance.durableSeq}</span>,
    <span key="connectivity">{instance.connectivity}</span>,
  );
  if (mobile) diagnostics.push(<span key="cost" data-testid="session-cost">{cost}</span>);
  if (structuredOnly && !showTerminal) {
    diagnostics.push(<span key="structured-only">structured-only — 无终端 tab</span>);
  }
  if (promoted) {
    diagnostics.push(
      <span key="promoted" data-testid="promoted-badge" title={
        instance.promotedAt ? `在终端里检测到 ${instance.kind}（${instance.promotedAt}）` : undefined
      }>
        {instance.kind} · promoted
      </span>,
    );
  }
  if (instance.launchedBy) {
    diagnostics.push(<LaunchedByMark key="launched-by" launchedBy={instance.launchedBy} />);
  }
  diagnostics.push(<ConnectionIndicator key="connection" status={connLabel} />);
  if (nativeRefShort !== "—") diagnostics.push(<span key="native">native {nativeRefShort}</span>);
  if (promoted) {
    diagnostics.push(
      <span
        key="binding"
        data-testid="transcript-binding"
        data-state={binding?.state ?? "unknown"}
        title={
          binding?.state === "degraded" && binding.reason
            ? binding.reason
            : promoted
              ? "promoted 终端的 transcript 绑定状态（hook / pid 文件 / argv / 手动）"
              : undefined
        }
      >
        {binding ? bindingChipText(binding) : "transcript 绑定中…"}
      </span>,
    );
  }
  if (journalStatus === "gap-backfill") diagnostics.push(<span key="note-gap">正在补事件</span>);
  if (journalStatus === "readonly-stale") diagnostics.push(<span key="note-readonly">只读</span>);
  if (status === "idle") diagnostics.push(<span key="note-idle">回合结束、进程仍在</span>);
  // model-pin-1 §5.4: the launch divergence is the Node's authoritative
  // `model_pin_mismatch` diagnostic (never recomputed here). Render each
  // recorded one verbatim in run details; a later /model changes the running
  // chip but leaves the historical diagnostic in place.
  // Projected records are durable and window-independent; the event window
  // adds a diagnostic that arrived before the projection landed.
  for (const mismatch of allModelPinMismatches(instance.modelPinMismatches, events)) {
    diagnostics.push(
      <span
        key={`model-pin-${mismatch.eventId ?? mismatch.observedAt ?? diagnostics.length}`}
        data-testid="run-details-model-pin"
        data-requested={mismatch.requested}
        data-observed={mismatch.observed}
      >
        请求模型 {mismatch.requested}，实际运行 {mismatch.observed}
      </span>,
    );
  }
  const diagnosticRows = diagnostics.flatMap((node, index) =>
    index === 0 ? [node] : [<span key={`sep-${index}`} className={session.dotSep}>·</span>, node],
  );

  // The ⋯ menu, in the fixed ui-spec §2.2 order. The view switch and Stop are
  // never here (D-040 (2)).
  const moreItems: MoreMenuItem[] = [
    {
      key: "run-details",
      testId: "run-details-summary",
      label: `运行详情 · ${diagnostics.length} 项`,
      icon: Info,
      checked: runDetailsOpen,
      onSelect: () => toggleRunDetails(!runDetailsOpen),
    },
  ];
  if (transcriptMounted) {
    moreItems.push(
      {
        key: "search",
        testId: "transcript-search-open",
        label: "搜索正文",
        icon: Search,
        onSelect: openTranscriptSearch,
      },
      {
        key: "collapse",
        testId: "collapse-all",
        label: "全部折叠",
        icon: ListCollapse,
        onSelect: collapseTranscript,
      },
    );
  }
  moreItems.push({
    key: "density",
    testId: "density-toggle",
    label: "紧凑工具卡",
    icon: Rows3,
    checked: hub.compact,
    data: { "data-mode": hub.compact ? "compact" : "full" },
    onSelect: () => hubStore.setCompact(!hub.compact),
  });
  if (showViewExtras && !filesInline) {
    moreItems.push({
      key: "files",
      testId: "files-toggle",
      label: "文件",
      icon: FileText,
      checked: resolvedView === "files",
      onSelect: toggleFiles,
    });
  }
  if (showViewExtras) {
    moreItems.push({
      key: "events",
      testId: "events-toggle",
      label: "原始事件",
      icon: ScrollText,
      checked: resolvedView === "events",
      onSelect: () => navigate(resolvedView === "events" ? backTo : `/s/${instance.id}/events`),
    });
  }
  // §2.2 item 7: the only annotation entry point. An archived task's session
  // is a read-only preview, so the item stays visible but inert; terminal
  // segments offer no annotations at all.
  if (annotationReadonly && resolvedView !== "tty" && resolvedView !== "events") {
    moreItems.push({
      key: "annotate",
      testId: "annotation-readonly-tag",
      label: "只读预览 · 不可批注",
      icon: MessageSquarePlus,
      disabled: true,
    });
  } else if (annotationAllowed) {
    moreItems.push({
      key: "annotate",
      testId: "annotation-add",
      label: "加批注",
      icon: MessageSquarePlus,
      onSelect: () => annotationPanel.openPanel(instance.id, "card", null),
    });
  }

  return (
    <div
      className={session.page}
      data-testid="session-page"
      data-status={status}
      data-lifecycle={instance.lifecycle}
      data-activity={activity}
      data-driver={instance.driver}
      data-kind={instance.kind}
      data-mode={instance.mode ?? "native"}
      data-view={resolvedView}
      data-journal={journalStatus}
      // t-annotations: anchor surfaces inside the transcript inherit the
      // owning session and the read-only flag (sessions of an archived task
      // are a read-only preview; terminal segments offer no annotations).
      data-annotation-instance={resolvedView === "tty" || resolvedView === "events" ? undefined : instance.id}
      data-annotation-readonly={annotationReadonly ? "1" : "0"}
      style={{ paddingBottom: offsetTop ? 0 : undefined }}
    >
      <WorkbenchSessionHeader
        mobile={mobile}
        title={title}
        taskTitle={sessionTask?.title ?? null}
        status={status}
        statusLabel={statusLabel}
        hostName={hostName}
        cost={cost}
        view={showTerminal ? (resolvedView === "tty" ? "tty" : "structured") : null}
        onView={(next) => navigate(`/s/${instance.id}/${next}`)}
        files={filesInline ? { active: resolvedView === "files", onToggle: toggleFiles } : null}
        onStop={
          status === "exited"
            ? null
            : () => {
                void hubStore.close(instance.id);
              }
        }
        more={
          <SessionMoreMenu open={moreOpen} onOpenChange={setMoreOpen} sheet={mobile} items={moreItems} />
        }
      />
      <RunDetails count={diagnostics.length} open={runDetailsOpen} onClose={closeRunDetails}>
        {diagnosticRows}
      </RunDetails>
      {routeDown ? (
        <div className={session.routeDown} role="alert" data-testid="session-api-route-down">
          <span className={session.routeDownTitle}>
            API 路由已断开（api-route-down）
          </span>
          {/* The route did not reroute: the strip still names it, and the
              operator re-dispatches rather than watching a silent fallback
              (D-035). */}
          <span>{routeClause}</span>
          <span className={session.routeDownDetail}>{routeDown}</span>
        </div>
      ) : null}
      <div
        ref={bodyRef}
        data-testid="session-body"
        className={resolvedView === "tty" || resolvedView === "structured" ? session.pane : undefined}
        style={
          resolvedView === "tty" || resolvedView === "structured"
            ? undefined
            : { flex: 1, overflow: "auto", minHeight: 0 }
        }
      >
        {resolvedView === "events" ? (
          <RawEvents events={events} />
        ) : resolvedView === "files" ? (
          <FilesView
            hostId={instance.hostId}
            workspaceId={instance.workspaceId}
            hostLabel={hostName}
            onBack={() => {
              if (location.key !== "default") navigate(-1);
              else navigate(backTo, { replace: true });
            }}
          />
        ) : resolvedView === "tty" ? (
          <TerminalView
            instance={instance}
            bottomChromeRef={setTtyChromeEl}
            onAttachFailed={(reason) => {
              hubStore.toast(reason);
              navigate(`/s/${instance.id}/structured`, { replace: true });
            }}
          />
        ) : snapshotLoading ? (
          <p style={{ padding: 16, color: "var(--mute)" }} data-testid="loading-snapshot">
            加载 snapshot…
          </p>
        ) : genericPty ? (
          <ScreenView instance={instance} events={events} />
        ) : (
          <Transcript
            ref={transcriptRef}
            events={events}
            bubbles={bubbles}
            compact={hub.compact}
            earlierFloor={hub.earlierFloor}
            journalStatus={journalStatus}
            steerHeld={steerHeldControl(instance.kind, composerPhase, instance.capabilities)}
            onSteerHeld={async (_iid, id) => {
              const landed = await hubStore.steerHeld(instance.id, id);
              // 已打断 is a receipt for an interrupt that actually landed.
              if (landed) setInterrupted(true);
              return landed;
            }}
            onRetryJournal={() => {
              void hubStore.catchup(instance.id);
            }}
          />
        )}
      </div>
      {resolvedView === "tty" || resolvedView === "events" ? (
        // Terminal segments and raw events carry no dock, but an ended session
        // still offers its one resume entry under the pane.
        endedBar ? <div className={session.endedDock}>{endedBar}</div> : null
      ) : <div ref={setDockEl} className={session.dock} data-testid="session-dock">
        {/* Zero-flow floating chip row anchored at the dock top (c-composerpop
            r2/r3): rests just above the composer over the transcript edge and
            never shrinks the session body's measured viewport share. */}
        <div className={annCss.floatLayer}>
          <div data-testid="annotation-dock" className={annCss.annotationBar}>
            <AnnotationBadge instanceId={instance.id} readonly={annotationReadonly} />
            {annotationAllowed ? (
              <button
                type="button"
                className={annCss.badge}
                data-testid="annotation-add"
                onClick={() => annotationPanel.openPanel(instance.id, "card", null)}
              >
                ＋ 加批注
              </button>
            ) : annotationReadonly ? (
              <span className={annCss.readonlyTag} data-testid="annotation-readonly-tag">
                只读预览 · 不可批注
              </span>
            ) : null}
          </div>
        </div>
        {pending.length > 0 ? (
          <div className={session.pendingArea} data-testid="pending-area">
            {pending.map((item) =>
              item.kind === "question" ? (
                <QuestionForm
                  key={item.id}
                  interaction={item}
                  busy={sending}
                  onRespond={(answer) => {
                    setSending(true);
                    void hubStore.respond(item.id, answer).finally(() => setSending(false));
                  }}
                />
              ) : item.kind === "elicitation" ? (
                <ElicitationCard
                  key={item.id}
                  interaction={item}
                  busy={sending}
                  onRespond={(answer) => {
                    setSending(true);
                    void hubStore.respond(item.id, answer).finally(() => setSending(false));
                  }}
                />
              ) : (
                <ApprovalCard
                  key={item.id}
                  interaction={item}
                  busy={sending}
                  onRespond={(answer) => {
                    setSending(true);
                    void hubStore.respond(item.id, answer).finally(() => setSending(false));
                  }}
                />
              ),
            )}
          </div>
        ) : null}
        <SessionNotifications key={`notes-${instance.id}`} instanceId={instance.id} events={events} />
        {/* An ended SESSION says so once, in the EndedBar; the strip's
            「回合结束」 would repeat it. An ended TURN of a live session keeps
            the strip. */}
        {endedBar ? null : (
          <LiveStatusStrip
            events={events}
            instance={instance}
            nativeRef={instance.nativeRef}
            hasPending={pending.length > 0}
            decision={turnDecision}
            onInterrupt={() => hubStore.cancel(instance.id)}
          />
        )}
        <TaskTrack tasks={tasks} />
        {genericPty ? (
          <div className={session.keys} data-testid="keys-row">
            {(["enter", "esc", "ctrl+c"] as const).map((key) => (
              <button
                key={key}
                type="button"
                className={session.keyBtn}
                data-testid={`keys-${key === "ctrl+c" ? "ctrl-c" : key}`}
                disabled={status === "exited"}
                onClick={() => {
                  void hubStore.sendKeys(instance.id, key);
                }}
              >
                {key}
              </button>
            ))}
          </div>
        ) : null}
        <AnnotationPanel
          instanceId={instance.id}
          taskId={sessionTask?.id ?? null}
          taskTitle={sessionTask?.title ?? null}
          readonly={annotationReadonly}
        />
        {/* An ended session mounts no Composer: the EndedBar is its one
            surface (and the one resume entry). */}
        {endedBar ?? <Composer
          key={instance.id}
          instanceId={instance.id}
          mobile={mobile}
          sending={sending}
          // c-steer: a pending question no longer hard-disables the composer —
          // typed messages hold with「待回答后送出」and flush once it resolves.
          disabled={status === "exited"}
          phase={composerPhase}
          capabilities={instance.capabilities}
          interrupted={interrupted}
          onInterruptedChange={setInterrupted}
          held={heldBubbles.map((b) => ({
            id: b.clientRequestId,
            text: b.text,
            reason: b.holdReason ?? "turn",
            holder: "remuda" as const,
          }))}
          onHold={(text, reason, refs, staged) => {
            // Client-held rows bypass onSend: fold the drafts in at hold time
            // so the prefix rides this queued delivery in order. The hold is
            // local-only (no POST), so clearing cannot fail.
            const composed = composeWithAnnotations(instance.id, text);
            if (composed.count > 0) annotationPanel.clear(instance.id);
            const previews = staged
              .filter((item) => item.objectId)
              .map((item) => ({
                objectId: item.objectId as string,
                name: item.name,
                previewUrl: item.previewUrl,
                kind: item.kind,
                mediaType: item.mediaType,
                size: item.size,
              }));
            hubStore.hold(instance.id, composed.text, reason, refs, previews);
          }}
          onRetractHeld={(id) => hubStore.retract(id)}
          onSteerHeld={(id) => hubStore.steerHeld(instance.id, id)}
          onFlushHeld={() => hubStore.flushHeld(instance.id)}
          onInterrupt={() => hubStore.cancel(instance.id)}
          permissionMode={
            genericPty ? ptyYoloChipLabel(instance.kind) : hub.permissionMode
          }
          launchPermissionMode={
            genericPty ? undefined : hub.launchPermissionMode
          }
          permissionEffective={genericPty ? null : hub.permissionEffective}
          permissionPending={genericPty ? null : hub.permissionPending}
          kind={instance.kind}
          model={hub.model}
          launchModel={instance.model ?? null}
          models={hub.models ?? undefined}
          modelEffective={hub.modelEffective?.id ?? null}
          modelPending={hub.modelPending}
          modelSelectionPath={hub.modelEffective?.selectionPath ?? null}
          modelCatalog={hub.modelCatalog}
          effort={hub.effort}
          effortEffective={hub.effortEffective}
          effortPending={hub.effortPending}
          contextLabel={(() => {
            const pct = contextPercent(usage, instance.kind);
            return pct == null ? null : `${pct}%`;
          })()}
          usageRollup={hub.usageRollup}
          onPermission={
            genericPty || instance.kind !== "claude"
              ? undefined
              : (mode) => {
                  void hubStore.setPermission(instance.id, mode);
                }
          }
          effortDisabled={status === "exited" || instance.ownership === "observed-only"}
          onEffort={(next) => {
            void hubStore.setEffort(instance.id, next);
          }}
          // The store owns the failure mouth: it reverts modelPending and
          // toasts the Hub/Node reason. Return the promise (never void it) so
          // a rejected configure is not an unhandled rejection and the
          // Composer can await it for pending UI.
          onModel={(next) => hubStore.setModel(instance.id, next)}
          onSend={async (text, attachments, staged, mode) => {
            setSending(true);
            try {
              // D-050 §7: annotation drafts ride this send as a structured
              // prompt prefix — no wire field, no table. They clear only when
              // the send landed (a failed POST keeps them as 待确认 drafts).
              const composed = composeWithAnnotations(instance.id, text);
              const sentText = composed.text;
              // D-027: the bubble keeps the local blob URLs so the sent
              // message shows thumbnails; the Hub does not echo attachments
              // back onto the journal yet.
              const previews = (staged ?? [])
                .filter((item) => item.objectId)
                .map((item) => ({
                  objectId: item.objectId as string,
                  name: item.name,
                  previewUrl: item.previewUrl,
                  kind: item.kind,
                  mediaType: item.mediaType,
                  size: item.size,
                }));
              const landed = await hubStore.send(
                instance.id,
                sentText,
                attachments ?? [],
                previews,
                mode,
              );
              if (landed !== false && composed.count > 0) {
                annotationPanel.clear(instance.id);
              }
              return landed;
            } finally {
              setSending(false);
            }
          }}
        />}
      </div>}
    </div>
  );
}
