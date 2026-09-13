import { useEffect, useState } from "react";
import { Link, Navigate, useNavigate, useParams } from "react-router-dom";
import { ConnectionIndicator } from "../components/ConnectionIndicator";
import { StateDot } from "../components/StateDot";
import { Button } from "../components/Button";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Composer } from "../features/session/Composer";
import { contextPercent } from "../features/session/effort";
import { ptyYoloChipLabel } from "../lib/sessionOptions";
import { useHostViews } from "../features/hosts";
import { Transcript } from "../features/session/Transcript";
import { TaskTrack } from "../features/session/TaskTrack";
import { RawEvents } from "../features/session/RawEvents";
import { assembleTranscript, collectTasks, compactTranscript } from "../features/session/assemble";
import { canShowTerminal, isTtyLabFixtureId, resolveTtyLabInstance, TerminalView } from "../features/session/tty";
import { ScreenView } from "../features/session/ScreenView";
import { nativeShort, isGenericPty, projectStatus, uiMode } from "../lib/status";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import ui from "../styles/ui.module.css";
import session from "../features/session/session.module.css";

export function SessionPage({
  view = "auto",
}: {
  view?: "auto" | "structured" | "tty" | "files" | "events";
}) {
  const { instanceId = "" } = useParams();
  const hub = useHub();
  const navigate = useNavigate();
  const { mobile, offsetTop } = useWorkbenchViewport();
  const [sending, setSending] = useState(false);
  const instance = hub.instances.find((i) => i.id === instanceId) ?? resolveTtyLabInstance(instanceId);
  const hostViews = useHostViews(hub.hosts, hub.instances);
  const followed = Boolean(hub.events[instanceId] || hub.journalStatus[instanceId]);

  useEffect(() => {
    if (instanceId && !isTtyLabFixtureId(instanceId)) void hubStore.follow(instanceId);
  }, [instanceId]);

  const events = hub.events[instanceId] ?? [];
  const pending = hub.interactions.filter((i) => i.instanceId === instanceId && i.state === "pending");
  const status = instance ? projectStatus(instance) : "unknown";
  const showTerminal = instance ? canShowTerminal(instance) : false;
  const resolvedView = view === "auto" ? (showTerminal ? "tty" : "structured") : view;
  const journalStatus = hub.journalStatus[instanceId] ?? (followed ? "live" : "live");
  const bubbles = hub.bubbles.filter((b) => b.instanceId === instanceId && b.state !== "settled");
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
  const canResume = instance.capabilities.capabilities.resume?.state === "supported";
  const connLabel = journalStatus === "live" ? hub.connection : journalStatus;
  const workspace = hubStore.workspaceOf(instance.workspaceId)?.label;
  const title = hubStore.titleOf(instance.id);
  const structuredOnly = uiMode(instance) === "structured-only";
  const genericPty = isGenericPty(instance);
  const activity = instance.activity.state === "known" ? instance.activity.value : instance.activity.state;

  return (
    <div
      className={session.page}
      data-testid="session-page"
      data-status={status}
      data-lifecycle={instance.lifecycle}
      data-activity={activity}
      data-driver={instance.driver}
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
          <span className={session.status}>
            <StateDot status={status} />
            {status}
          </span>
          <span className={session.spacer} />
          {showTerminal ? (
            <span className={ui.row}>
              <Link to={`/s/${instance.id}/tty`} aria-current={resolvedView === "tty" ? "page" : undefined}>
                终端
              </Link>
              <Link
                to={`/s/${instance.id}/structured`}
                aria-current={resolvedView === "structured" ? "page" : undefined}
              >
                结构
              </Link>
            </span>
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
          {resolvedView === "structured" ? (
            <>
              <button type="button" className={`${session.headBtn} ${session.deskOnly}`} onClick={() => navigate(`/s/${instance.id}/files`)}>
                文件
              </button>
              <button type="button" className={session.headBtn} onClick={() => navigate(`/s/${instance.id}/events`)}>
                原始事件
              </button>
            </>
          ) : null}
          {status === "exited" ? (
            canResume ? (
              <Button
                variant="primary"
                onClick={() => {
                  void hubStore.resume(instance.id);
                }}
              >
                Resume
              </Button>
            ) : (
              <Link to={`/sessions/new?host=${instance.hostId}&workspace=${instance.workspaceId}`}>开新会话继承 cwd</Link>
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
          <span>{instance.driver}</span>
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
          {journalStatus === "gap-backfill" ? " · 正在补事件" : ""}
          {journalStatus === "readonly-stale" ? " · 只读" : ""}
          {status === "idle" ? " · 回合结束、进程仍在" : ""}
        </div>
      </header>
      <div
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
          <p style={{ padding: 16, color: "var(--mute)" }}>文件 / diff 栏占位。空间不够时走这条全屏路由。</p>
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
            onRetryJournal={() => {
              void hubStore.catchup(instance.id);
            }}
          />
        )}
      </div>
      {resolvedView === "tty" || resolvedView === "events" ? null : <div className={session.dock}>
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
          disabled={status === "exited" || pending.length > 0}
          permissionMode={
            genericPty ? ptyYoloChipLabel(instance.kind) : hubStore.permissionModeOf(instance.id)
          }
          kind={instance.kind}
          model={hubStore.modelOf(instance.id, instance.kind)}
          effort={hubStore.effortOf(instance.id, instance.kind)}
          contextLabel={(() => {
            const pct = contextPercent(usage, instance.kind);
            return pct == null ? null : `${pct}%`;
          })()}
          hostLabel={hubStore.hostName(instance.hostId)}
          hostCli={hostViews.find((h) => h.id === instance.hostId)?.cli ?? []}
          onPermission={
            genericPty
              ? undefined
              : (mode) => {
                  void hubStore.configure(instance.id, mode);
                }
          }
          effortDisabled={status === "exited" || instance.ownership === "observed-only"}
          onEffort={(next) => {
            void hubStore.setEffort(instance.id, next);
          }}
          onModel={(next) => {
            void hubStore.setModel(instance.id, next);
          }}
          onSend={async (text: string) => {
            setSending(true);
            try {
              await hubStore.send(instance.id, text);
            } finally {
              setSending(false);
            }
          }}
        />
      </div>}
    </div>
  );
}
