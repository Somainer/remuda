import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Link, Navigate, useLocation, useNavigate, useParams } from "react-router-dom";
import { ConnectionIndicator } from "../components/ConnectionIndicator";
import { StateDot } from "../components/StateDot";
import { Button } from "../components/Button";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { ElicitationCard } from "../features/approvals/ElicitationCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Composer } from "../features/session/Composer";
import { steerHeldControl } from "../features/composer/state";
import { LaunchedByMark } from "../features/session/LaunchedBy";
import { contextPercent } from "../features/session/effort";
import { ptyYoloChipLabel } from "../lib/sessionOptions";
import { Transcript } from "../features/session/Transcript";
import { LiveStatusStrip } from "../features/session/live/LiveStatusStrip";
import { TaskTrack } from "../features/session/TaskTrack";
import { RawEvents } from "../features/session/RawEvents";
import { assembleTranscript, collectTasks, compactTranscript } from "../features/session/assemble";
import { canShowTerminal, hasStructuredSignal, isTtyLabFixtureId, resolveTtyLabInstance, TerminalView } from "../features/session/tty";
import { ScreenView } from "../features/session/ScreenView";
import { ViewSwitch } from "../features/session/ViewSwitch";
import { nativeShort, isGenericPty, isPromoted, projectStatus, uiMode, UI_STATUS_LABEL } from "../lib/status";
import { projectCommandStatus } from "../lib/commandStatus";
import { bindingChipText, transcriptBinding } from "../lib/transcriptBinding";
import type { ResumeMode } from "../lib/api";
import { hubStore, useHub } from "../lib/store";
import type { Id } from "../types/wire";
import { useWorkbenchViewport } from "../lib/viewport";
import { useSpaceWorkbench } from "../features/spaces/useSpaceWorkbench";
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
  const { active: space, newHref } = useSpaceWorkbench();
  const navigate = useNavigate();
  const location = useLocation();
  const { mobile, offsetTop } = useWorkbenchViewport();
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
  // D-028 §6 composer phase. `starting` behaves like idle (one send box);
  // only a live working/blocked turn exposes steer/queue/interrupt.
  const composerPhase =
    status === "blocked" ? "blocked" : status === "working" ? "working" : status === "exited" ? "exited" : "idle";
  const nodeRestarted = instance?.lastError === "node-epoch-changed";
  const resolvedView = view === "auto" ? baseView : view;

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
  const tasks = collectTasks(compactTranscript(assembleTranscript(events, bubbles), hub.compact));
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
      style={{ paddingBottom: offsetTop ? 0 : undefined }}
    >
      <header className={session.header}>
        <div className={session.headRow}>
          {mobile ? (
            <Link className={session.back} to="/sessions" aria-label="返回">
              ←
            </Link>
          ) : null}
          <h1 className={session.title}>{workspace ? `${workspace} / ${title}` : title}</h1>
          <span className={session.status} data-testid="session-status-label">
            <StateDot status={status} />
            {statusLabel}
          </span>
          {promoted ? (
            <span className={session.status} data-testid="promoted-badge" title={
              instance.promotedAt ? `在终端里检测到 ${instance.kind}（${instance.promotedAt}）` : undefined
            }>
              {instance.kind} · promoted
            </span>
          ) : null}
          <LaunchedByMark launchedBy={instance.launchedBy} />
          <span className={session.spacer} />
          {showTerminal ? (
            <ViewSwitch
              value={resolvedView === "tty" ? "tty" : "structured"}
              onChange={(next) => navigate(`/s/${instance.id}/${next}`)}
            />
          ) : null}
          <button
            type="button"
            className={`${session.headBtn} ${hub.compact ? session.headBtnOn : ""}`}
            data-testid="density-toggle"
            data-mode={hub.compact ? "compact" : "full"}
            onClick={() => hubStore.setCompact(!hub.compact)}
          >
            {hub.compact ? "Compact" : "Full"}
          </button>
          {resolvedView === "structured" || resolvedView === "files" || resolvedView === "events" ? (
            <>
              {/* 文件 was deskOnly, which hid the only entry to the fullscreen
                  files route at ≤767px. It is now reachable on every width. */}
              <button
                type="button"
                className={resolvedView === "files" ? session.headBtnActive : session.headBtn}
                data-testid="files-toggle"
                aria-pressed={resolvedView === "files"}
                onClick={() => (resolvedView === "files" ? navigate(backTo) : openFiles())}
              >
                文件
              </button>
              <button
                type="button"
                className={resolvedView === "events" ? session.headBtnActive : session.headBtn}
                data-testid="events-toggle"
                aria-pressed={resolvedView === "events"}
                onClick={() => navigate(resolvedView === "events" ? backTo : `/s/${instance.id}/events`)}
              >
                原始事件
              </button>
            </>
          ) : null}
          {status === "exited" ? (
            canResume ? (
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
              <Link to={newHref}>开新会话继承 cwd</Link>
            )
          ) : (
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
          )}
        </div>
        <div className={session.meta} data-testid="session-meta">
          <span className={session.metaHost}>{hubStore.hostName(instance.hostId)}</span>
          <span className={session.dotSep}>·</span>
          <span data-testid="session-driver">
            {promoted ? `${instance.driver} · promoted` : instance.driver}
          </span>
          <span className={session.dotSep}>·</span>
          <span data-testid="session-delegation">{instance.delegation ?? "none"}</span>
          <span className={session.dotSep}>·</span>
          <span data-testid="session-provider">{instance.providerProfileId ?? "none"}</span>
          {instance.providerSourceHint ? (
            <>
              <span className={session.dotSep}>·</span>
              <span data-testid="session-provider-source">{instance.providerSourceHint}</span>
            </>
          ) : null}
          <span className={session.dotSep}>·</span>
          <span data-testid="session-lifecycle">{instance.lifecycle}</span>
          <span className={session.dotSep}>·</span>
          <span>seq {events.at(-1)?.seq ?? instance.durableSeq}</span>
          <span className={session.dotSep}>·</span>
          <span>{instance.connectivity}</span>
          <span className={session.dotSep}>·</span>
          <span>{cost}</span>
          {structuredOnly && !showTerminal && !mobile ? (
            <>
              <span className={session.dotSep}>·</span>
              <span>structured-only — 无终端 tab</span>
            </>
          ) : null}
          <ConnectionIndicator status={connLabel} />
          {nativeShort(instance) !== "—" ? (
            <>
              <span className={session.dotSep}>·</span>
              <span>native {nativeShort(instance)}</span>
            </>
          ) : null}
          {promoted ? (
            <>
              <span className={session.dotSep}>·</span>
              <span
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
              </span>
            </>
          ) : null}
          {journalStatus === "gap-backfill" ? " · 正在补事件" : ""}
          {journalStatus === "readonly-stale" ? " · 只读" : ""}
          {status === "idle" ? " · 回合结束、进程仍在" : ""}
        </div>
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
        <LiveStatusStrip
          events={events}
          nativeRef={instance.nativeRef}
          onInterrupt={() => hubStore.cancel(instance.id)}
        />
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
            hubStore.hold(instance.id, text, reason, refs, previews);
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
          models={hubStore.modelListOf(instance.id) ?? undefined}
          modelEffective={hubStore.modelEffectiveOf(instance.id)?.id ?? null}
          modelPending={hubStore.modelPendingOf(instance.id)}
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
          onModel={(next) => {
            void hubStore.setModel(instance.id, next);
          }}
          onSend={async (text, attachments, staged, mode) => {
            setSending(true);
            try {
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
              return await hubStore.send(instance.id, text, attachments ?? [], previews, mode);
            } finally {
              setSending(false);
            }
          }}
        />
      </div>}
    </div>
  );
}
