import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Link, Navigate, useLocation, useNavigate, useParams } from "react-router-dom";
import { ConnectionIndicator } from "../components/ConnectionIndicator";
import { StateDot } from "../components/StateDot";
import { Button } from "../components/Button";
import { Sheet } from "../components/Sheet";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { ElicitationCard } from "../features/approvals/ElicitationCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Composer } from "../features/session/Composer";
import { steerHeldControl } from "../features/composer/state";
import { LaunchedByMark } from "../features/session/LaunchedBy";
import { RunDetails } from "../features/session/RunDetails";
import { contextPercent } from "../features/session/effort";
import { ptyYoloChipLabel } from "../lib/sessionOptions";
import { Transcript } from "../features/session/Transcript";
import { LiveStatusStrip } from "../features/session/live/LiveStatusStrip";
import { projectTurnDecision } from "../features/session/live/turnDecision";
import { useNow } from "../features/session/live/useElapsed";
import { SessionNotifications } from "../features/session/notifications/SessionNotifications";
import { TaskTrack } from "../features/session/TaskTrack";
import {
  AnnotationBadge,
  AnnotationPanel,
  useAnnotationsContext,
  useSessionTask,
} from "../features/tasks/AnnotationPanel";
import { composeWithAnnotations } from "../features/tasks/annotations";
import annCss from "../features/tasks/annotation.module.css";
import { RawEvents } from "../features/session/RawEvents";
import { assembleTranscript, collectTasks, compactTranscript } from "../features/session/assemble";
import { readDismissedWorkflows } from "../features/session/workflowDismiss";
import { canShowTerminal, hasStructuredSignal, isTtyLabFixtureId, resolveTtyLabInstance, TerminalView } from "../features/session/tty";
import { ScreenView } from "../features/session/ScreenView";
import { ViewSwitch } from "../features/session/ViewSwitch";
import { nativeShort, isGenericPty, isPromoted, projectStatus, uiMode, UI_STATUS_LABEL } from "../lib/status";
import { apiRouteClause, apiRouteKind, routeDownMessage } from "../lib/apiRoute";
import { projectCommandStatus } from "../lib/commandStatus";
import { bindingChipText, transcriptBinding } from "../lib/transcriptBinding";
import type { ResumeMode } from "../lib/api";
import { hubStore, useHub } from "../lib/store";
import type { Id } from "../types/wire";
import { useWorkbenchViewport } from "../lib/viewport";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
import { SpacesMobile } from "../features/spaces/SpacesMobile";
import { readSessionView, writeSessionView, type SessionView } from "../lib/viewPref";
import { FilesView } from "../features/files/FilesView";
import session from "../features/session/session.module.css";

export function SessionPage({
  view = "auto",
}: {
  view?: "auto" | "structured" | "tty" | "files" | "events";
}) {
  const { instanceId = "" } = useParams();
  const hub = useHub();
  const annotationPanel = useAnnotationsContext();
  const workbench = useSpaceWorkbench();
  const { active: space, newHref } = workbench;
  const navigate = useNavigate();
  const location = useLocation();
  const { mobile, offsetTop } = useWorkbenchViewport();
  // D-040 phone fold: below this width the header cannot hold every control
  // without pushing Stop past the viewport edge, so Compact / 文件 / 原始事件
  // move into the ⋯ sheet. Width-keyed (not coarsePointer) so a narrow
  // window without touch keeps the same layout — folding is a layout question.
  // 767px deliberately stays inline: the whole row fits there and the touch
  // contract probes that exact width.
  const [crowded, setCrowded] = useState(false);
  useEffect(() => {
    if (!mobile || typeof window.matchMedia !== "function") {
      setCrowded(false);
      return;
    }
    const media = window.matchMedia("(max-width: 640px)");
    const update = () => setCrowded(media.matches);
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, [mobile]);
  const [moreOpen, setMoreOpen] = useState(false);
  const moreRef = useRef<HTMLButtonElement | null>(null);
  const [sendingIds, setSendingIds] = useState<string[]>([]);
  const sending = sendingIds.includes(instanceId);
  const setSending = (value: boolean) => setSendingIds((ids) => value ? [...new Set([...ids, instanceId])] : ids.filter((id) => id !== instanceId));
  const [resuming, setResuming] = useState(false);
  // 已打断 receipt shared by the composer controls and the transcript held-row
  // 插队发送 button, so the gesture reads the same from either place. Reset on
  // session switch; the composer also clears it on the next turn / after 4 s.
  const [interrupted, setInterrupted] = useState(false);
  useEffect(() => setInterrupted(false), [instanceId]);
  const instance = hub.instances.find((i) => i.id === instanceId) ?? resolveTtyLabInstance(instanceId);
  // t-annotations: a session of an archived task is a read-only preview — no
  // badge/entry point and anchor selection raises nothing.
  const sessionTask = useSessionTask(instanceId, (instance as { taskId?: string | null } | undefined)?.taskId);
  const annotationReadonly = sessionTask?.archivedAt != null;
  const followed = Boolean(hub.events[instanceId] || hub.journalStatus[instanceId]);
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

  const events = hub.events[instanceId] ?? [];
  const pending = hub.interactions.filter((i) => i.instanceId === instanceId && i.state === "pending");
  const status = instance ? projectStatus(instance) : "unknown";
  // The turn-end decision folds every channel (hook latch, screen, transcript
  // tail, pending interactions), not the hook latch alone — so a turn whose
  // Stop hook never lands still ends once the screen/pty says idle. It is the
  // one decision the strip and the composer share, which is what lets a held
  // prompt flush on the same boundary the clock stops on. A 1 Hz tick drives
  // the hook-freshness judgement (the deciding signal after a turn goes quiet
  // is elapsed time, not a new event).
  const liveNow = useNow(true);
  const turnDecision = useMemo(
    () => projectTurnDecision(events, instance?.nativeRef, pending.length > 0, liveNow),
    [events, instance?.nativeRef, pending.length, liveNow],
  );
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
  const nodeRestarted = instance?.lastError === "node-epoch-changed";
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
  const journalStatus = hub.journalStatus[instanceId] ?? (followed ? "live" : "live");
  const bubbles = hub.bubbles.filter((b) => b.instanceId === instanceId && b.state !== "settled");
  // c-steer: Remuda-held queue rows (Enter while busy / while a question is
  // pending). Posted in order by flushHeld when the wait ends.
  const heldBubbles = hubStore.heldBubbles(instanceId);
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
  const snapshotLoading = Boolean(instance) && hub.events[instanceId] === undefined && !isTtyLabFixtureId(instanceId);

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
  const connLabel = journalStatus === "live" ? hub.connection : journalStatus;
  const workspace = space?.name;
  const title = hubStore.titleOf(instance.id);
  const structuredOnly = uiMode(instance) === "structured-only";
  const genericPty = isGenericPty(instance);
  const promoted = isPromoted(instance);
  const binding = promoted ? transcriptBinding(events) : null;
  const activity = instance.activity.state === "known" ? instance.activity.value : instance.activity.state;
  const hostName = hubStore.hostName(instance.hostId);
  const nativeRefShort = nativeShort(instance);
  const showViewExtras = resolvedView === "structured" || resolvedView === "files" || resolvedView === "events";
  const closeMore = () => setMoreOpen(false);
  const renderDensity = (menu: boolean) => (
    <button
      key="density"
      type="button"
      role={menu ? "menuitem" : undefined}
      className={`${session.headBtn} ${hub.compact ? session.headBtnOn : ""} ${menu ? session.menuBtn : ""}`}
      data-testid="density-toggle"
      data-mode={hub.compact ? "compact" : "full"}
      onClick={() => {
        hubStore.setCompact(!hub.compact);
        if (menu) closeMore();
      }}
    >
      {hub.compact ? "Compact" : "Full"}
    </button>
  );
  const renderFiles = (menu: boolean) => (
    <button
      key="files"
      type="button"
      role={menu ? "menuitem" : undefined}
      className={`${resolvedView === "files" ? session.headBtnActive : session.headBtn} ${menu ? session.menuBtn : ""}`}
      data-testid="files-toggle"
      aria-pressed={resolvedView === "files"}
      onClick={() => {
        if (resolvedView === "files") navigate(backTo);
        else openFiles();
        if (menu) closeMore();
      }}
    >
      文件
    </button>
  );
  const renderEvents = (menu: boolean) => (
    <button
      key="events"
      type="button"
      role={menu ? "menuitem" : undefined}
      className={`${resolvedView === "events" ? session.headBtnActive : session.headBtn} ${menu ? session.menuBtn : ""}`}
      data-testid="events-toggle"
      aria-pressed={resolvedView === "events"}
      onClick={() => {
        navigate(resolvedView === "events" ? backTo : `/s/${instance.id}/events`);
        if (menu) closeMore();
      }}
    >
      原始事件
    </button>
  );
  const resumeControl = canResume ? (
    <span className={session.headRow} data-testid="resume-control">
      {/* D-026: resume continues the same native session on a NEW
          instance, so both targets navigate away from this one. */}
      <Button
        variant="primary"
        disabled={resuming}
        onClick={() => {
          void startResume("structured");
        }}
      >
        继续（结构化）
      </Button>
      <button
        type="button"
        className={session.headBtn}
        data-testid="resume-terminal"
        disabled={resuming}
        onClick={() => {
          void startResume("terminal");
        }}
      >
        在终端中继续
      </button>
    </span>
  ) : (
    <Link to={newHref} className={session.inheritLink}>开新会话继承 cwd</Link>
  );
  // Two resume buttons cannot share the crowded 390px main row without
  // wrapping over the disclosure; on phones they take their own header row.
  const resumeOnOwnRow = status === "exited" && crowded && canResume;

  // The 运行详情 fields as one array: the summary advertises exactly the
  // number of items it can reveal (ui-spec §2.2), so the count is derived,
  // never hand-maintained. host/cost keep the desktop main row; on compact
  // (and coarse-pointer compact, which also sets `mobile` above 640px) the
  // disclosure is their only home — provenance and promotion move here too.
  const diagnostics: ReactNode[] = [];
  if (mobile) diagnostics.push(<span key="host" className={session.metaHost}>{hostName}</span>);
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
  if (mobile && promoted) {
    diagnostics.push(
      <span key="promoted" className={session.status} data-testid="promoted-badge" title={
        instance.promotedAt ? `在终端里检测到 ${instance.kind}（${instance.promotedAt}）` : undefined
      }>
        {instance.kind} · promoted
      </span>,
    );
  }
  if (mobile && instance.launchedBy) {
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
  const diagnosticRows = diagnostics.flatMap((node, index) =>
    index === 0 ? [node] : [<span key={`sep-${index}`} className={session.dotSep}>·</span>, node],
  );

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
      <header className={session.header}>
        <div className={session.headRow}>
          {mobile ? (
            <Link className={session.back} to="/sessions" aria-label="返回">
              ←
            </Link>
          ) : null}
          {/* D-040: on compact /s/:id* the whole chips strip folds into this
              one current-space chip; it opens the unchanged spaces drawer. */}
          {mobile ? (
            <span className={session.headSpaceChip}>
              <SpacesMobile
                variant="chip"
                spaces={workbench.spaces}
                active={workbench.active}
                prefs={workbench.prefs}
                instanceId={workbench.instanceId}
                onSelect={workbench.select}
              />
            </span>
          ) : null}
          {/* The chip already names the space, so the mobile title does not
              repeat the "space / " prefix. */}
          <h1 className={session.title} title={workspace ? `${workspace} / ${title}` : title}>
            {workspace && !mobile ? `${workspace} / ${title}` : title}
          </h1>
          <span
            className={session.status}
            data-testid="session-status-label"
            title={statusLabel}
          >
            <StateDot status={status} />
            {/* At crowded phone widths only the dot shows; the word stays in
                the DOM (tests, screen readers) and in the title tooltip. */}
            <span className={session.statusWord}>{statusLabel}</span>
          </span>
          {/* Provenance and promotion badges ride the desktop main row; on
              compact (including the coarse-pointer compact clause) they are
              rendered once, inside the 运行详情 disclosure. */}
          {!mobile ? (
            <span className={session.headBadges}>
              {promoted ? (
                <span className={session.status} data-testid="promoted-badge" title={
                  instance.promotedAt ? `在终端里检测到 ${instance.kind}（${instance.promotedAt}）` : undefined
                }>
                  {instance.kind} · promoted
                </span>
              ) : null}
              <LaunchedByMark launchedBy={instance.launchedBy} />
            </span>
          ) : null}
          {!mobile ? (
            <>
              <span className={session.hostChip} data-testid="session-host" title={`主机 ${hostName}`}>
                {hostName}
              </span>
              <span className={session.costChip} data-testid="session-cost">
                {cost}
              </span>
            </>
          ) : null}
          <span className={session.spacer} />
          {showTerminal ? (
            <ViewSwitch
              value={resolvedView === "tty" ? "tty" : "structured"}
              onChange={(next) => navigate(`/s/${instance.id}/${next}`)}
            />
          ) : null}
          {crowded ? null : renderDensity(false)}
          {crowded || !showViewExtras ? null : (
            <>
              {/* 文件/原始事件 stay inline whenever the row fits; only the
                  crowded phone fold moves them into ⋯ (D-040). */}
              {renderFiles(false)}
              {renderEvents(false)}
            </>
          )}
          {status === "exited" && !resumeOnOwnRow ? resumeControl : null}
          {status !== "exited" ? (
            <button
              type="button"
              className={session.stopBtn}
              aria-label="Stop"
              onClick={() => {
                void hubStore.close(instance.id);
              }}
            >
              {mobile ? "■" : "■ 停止"}
            </button>
          ) : null}
          {crowded ? (
            <>
              {/* The view switch and Stop are permanent main-row citizens;
                  this ⋯ only ever holds Compact / 文件 / 原始事件. */}
              <button
                type="button"
                className={session.moreBtn}
                data-testid="session-more-open"
                aria-label="更多会话操作"
                aria-haspopup="menu"
                aria-expanded={moreOpen}
                ref={moreRef}
                onClick={() => setMoreOpen((value) => !value)}
              >
                ⋯
              </button>
              <Sheet
                open={moreOpen}
                onClose={closeMore}
                variant="sheet"
                testId="session-more-sheet"
                returnFocusRef={moreRef}
              >
                <div className={session.moreMenu} role="menu" aria-label="会话操作">
                  {renderDensity(true)}
                  {showViewExtras ? renderFiles(true) : null}
                  {showViewExtras ? renderEvents(true) : null}
                </div>
              </Sheet>
            </>
          ) : null}
        </div>
        {resumeOnOwnRow ? (
          <div className={session.resumeRow} data-testid="resume-row">
            {resumeControl}
          </div>
        ) : null}
        <RunDetails count={diagnostics.length}>{diagnosticRows}</RunDetails>
      </header>
      {nodeRestarted ? (
        <div className={session.nodeRestart} data-testid="node-restart-banner">
          <span>Node 重启，会话已结束</span>
          <Button
            variant="primary"
            disabled={resuming || !canResume}
            data-testid="node-restart-resume"
            onClick={() => {
              void startResume("structured");
            }}
          >
            Resume
          </Button>
          {!canResume ? <span className={session.nodeRestartNote}>该会话没有可续接的 transcript</span> : null}
        </div>
      ) : null}
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
            hostLabel={hubStore.hostName(instance.hostId)}
            onBack={() => {
              if (location.key !== "default") navigate(-1);
              else navigate(backTo, { replace: true });
            }}
          />
        ) : resolvedView === "tty" ? (
          <TerminalView
            instance={instance}
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
            events={events}
            bubbles={bubbles}
            compact={hub.compact}
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
      {resolvedView === "tty" || resolvedView === "events" ? null : <div className={session.dock}>
        {/* Zero-flow floating chip row anchored at the dock top: it rests
            just above the composer (over the transcript edge) and never
            shrinks the session body's measured viewport share — visible
            whether the in-flow panel is open or not. */}
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
        <LiveStatusStrip
          events={events}
          nativeRef={instance.nativeRef}
          hasPending={pending.length > 0}
          decision={turnDecision}
          onInterrupt={() => hubStore.cancel(instance.id)}
        />
        <SessionNotifications key={`notes-${instance.id}`} instanceId={instance.id} events={events} />
        <TaskTrack tasks={tasks} />
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
        <Composer
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
            genericPty ? ptyYoloChipLabel(instance.kind) : hubStore.permissionModeOf(instance.id)
          }
          launchPermissionMode={
            genericPty ? undefined : hubStore.launchPermissionModeOf(instance.id)
          }
          permissionEffective={genericPty ? null : hubStore.permissionEffectiveOf(instance.id)}
          permissionPending={genericPty ? null : hubStore.permissionPendingOf(instance.id)}
          kind={instance.kind}
          model={hubStore.modelOf(instance.id, instance.kind)}
          modelRequested={hubStore.modelRequestedOf(instance.id, instance.kind)}
          models={hubStore.modelListOf(instance.id) ?? undefined}
          modelEffective={hubStore.modelEffectiveOf(instance.id)?.id ?? null}
          modelPending={hubStore.modelPendingOf(instance.id)}
          modelSelectionPath={hubStore.modelEffectiveOf(instance.id)?.selectionPath ?? null}
          modelCatalog={hubStore.modelCatalogOf(instance.id)}
          effort={hubStore.effortOf(instance.id, instance.kind)}
          effortEffective={hubStore.effortEffectiveOf(instance.id)}
          effortPending={hubStore.effortPendingOf(instance.id)}
          contextLabel={(() => {
            const pct = contextPercent(usage, instance.kind);
            return pct == null ? null : `${pct}%`;
          })()}
          usageRollup={hubStore.usageRollupOf(instance.id)}
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
        />
      </div>}
    </div>
  );
}
