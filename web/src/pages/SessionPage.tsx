import { useEffect, useState, type ReactNode } from "react";
import { Link, Navigate, useNavigate, useParams } from "react-router-dom";
import { ConnectionIndicator } from "../components/ConnectionIndicator";
import { StateDot } from "../components/StateDot";
import { Button } from "../components/Button";
import { ApprovalCard } from "../features/approvals/ApprovalCard";
import { QuestionForm } from "../features/approvals/QuestionForm";
import { Composer } from "../features/session/Composer";
import { Transcript } from "../features/session/Transcript";
import { TerminalView } from "../features/session/tty/TerminalView";
import type { Instance } from "../types/instance";
import { nativeShort, projectStatus, uiMode } from "../lib/status";
import { hubStore, useHub } from "../lib/store";
import { useWorkbenchViewport } from "../lib/viewport";
import { formatTokens } from "../lib/format";
import ui from "../styles/ui.module.css";
import type { UsagePayload } from "../types/observation";

function TtyPane({ instance }: { instance: Instance }) {
  const Pane = TerminalView as (props: { instance?: Instance; instanceId?: string; onAttachFailed?: (reason: string) => void }) => ReactNode;
  return (
    <Pane
      instance={instance}
      instanceId={instance.id}
      onAttachFailed={(reason) => hubStore.toast(reason)}
    />
  );
}

export function SessionPage({ view = "structured" }: { view?: "structured" | "tty" | "files" }) {
  const { instanceId = "" } = useParams();
  const hub = useHub();
  const navigate = useNavigate();
  const { mobile, offsetTop } = useWorkbenchViewport();
  const [sending, setSending] = useState(false);
  const instance = hub.instances.find((i) => i.id === instanceId) ?? resolveTtyLabInstance(instanceId);
  const followed = Boolean(hub.events[instanceId] || hub.journalStatus[instanceId]);

  useEffect(() => {
    if (instanceId && !isTtyLabFixtureId(instanceId)) void hubStore.follow(instanceId);
  }, [instanceId]);

  const events = hub.events[instanceId] ?? [];
  const pending = hub.interactions.filter((i) => i.instanceId === instanceId && i.state === "pending");
  const status = instance ? projectStatus(instance) : "unknown";
  const showTtyLab = instance ? canShowTtyLab(instance) : false;
  const journalStatus = hub.journalStatus[instanceId] ?? (followed ? "live" : "live");
  const bubbles = hub.bubbles.filter((b) => b.instanceId === instanceId && b.state !== "settled");
  const usageEvent = events.findLast((e) => e.kind === "usage");
  const usage = usageEvent?.payload as UsagePayload | undefined;
  const snapshotLoading = Boolean(instance) && hub.events[instanceId] === undefined;

  if (!instance && hub.ready) {
    return <p style={{ padding: 16 }}>会话不存在</p>;
  }
  if (!instance) return <p style={{ padding: 16 }}>加载 snapshot…</p>;

  if (view === "tty" && !showTtyLab) {
    hubStore.toast("终端实验页仅在 VITE_DEV_TTY=1 且 capabilities.ttyAttach 时可用");
    return <Navigate to={`/s/${instanceId}`} replace />;
  }

  const cost = usage && usage.cost.state === "known" ? `$${usage.cost.value.amount}` : "—";
  const inTok = usage ? formatTokens(usage.inputTokens) : null;
  const outTok = usage ? formatTokens(usage.outputTokens) : null;
  const canResume = instance.capabilities.capabilities.resume?.state === "supported";
  const connLabel = journalStatus === "live" ? hub.connection : journalStatus;

  return (
    <div
      data-testid="session-page"
      style={{ display: "flex", flexDirection: "column", minHeight: "100%", paddingBottom: offsetTop ? 0 : undefined }}
    >
      <header style={{ display: "flex", gap: 8, alignItems: "center", padding: "8px 12px", borderBottom: "1px solid var(--line)", flexWrap: "wrap" }}>
        {mobile ? (
          <Link to="/sessions" aria-label="返回">
            ←
          </Link>
        ) : null}
        <StateDot status={status} />
        <strong style={{ flex: 1 }}>{hubStore.titleOf(instance.id)}</strong>
        <span className={ui.pill}>{hubStore.workspaceOf(instance.workspaceId)?.label}</span>
        <span className={ui.pill}>{hubStore.hostName(instance.hostId)}</span>
        <span className={ui.pill}>{instance.driver}</span>
        <ConnectionIndicator status={connLabel} />
        {showTtyLab ? (
          <span className={ui.row}>
            <Link to={`/s/${instance.id}`}>结构</Link>
            <Link to={`/s/${instance.id}/tty`} aria-current={view === "tty" ? "page" : undefined}>
              终端
            </Link>
          </span>
        ) : null}
        <Button
          variant="ghost"
          onClick={() => hubStore.setCompact(!hub.compact)}
        >
          {hub.compact ? "Compact 开" : "Compact 关"}
        </Button>
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
          <Button
            variant="danger"
            onClick={() => {
              void hubStore.close(instance.id);
            }}
          >
            Stop
          </Button>
        )}
      </header>
      <div className={ui.listMeta} style={{ padding: "4px 12px" }} data-testid="session-meta">
        seq {events.at(-1)?.seq ?? instance.durableSeq} · connectivity={instance.connectivity} · {cost}
        {inTok && outTok ? ` · in ${inTok} / out ${outTok}` : ""} · native {nativeShort(instance)}
        {journalStatus === "gap-backfill" ? " · 正在补事件" : ""}
        {journalStatus === "readonly-stale" ? " · 只读" : ""}
        {status === "idle" ? " · 回合结束、进程仍在" : ""}
      </div>
      <div style={{ flex: 1, overflow: "auto" }}>
        {view === "files" ? (
          <p style={{ padding: 16, color: "var(--mute)" }}>文件 / diff 栏占位。空间不够时走这条全屏路由。</p>
        ) : view === "tty" ? (
          <TtyPane instance={instance} />
        ) : snapshotLoading ? (
          <p style={{ padding: 16, color: "var(--mute)" }} data-testid="loading-snapshot">
            加载 snapshot…
          </p>
        ) : (
          <Transcript events={events} bubbles={bubbles} compact={hub.compact} />
        )}
      </div>
      {view === "tty" ? null : <div style={{ padding: 12, borderTop: "1px solid var(--line)" }}>
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
        {status === "blocked" ? null : (
          <Composer
            key={instance.id}
            instanceId={instance.id}
            mobile={mobile}
            sending={sending}
            disabled={status === "exited"}
            permissionMode={hubStore.permissionModeOf(instance.id)}
            onPermission={(mode) => {
              void hubStore.configure(instance.id, mode);
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
        )}
      </div>}
      {view === "structured" ? (
        <div style={{ padding: "0 12px 12px" }}>
          <Button variant="ghost" onClick={() => navigate(`/s/${instance.id}/files`)}>
            文件
          </Button>
        </div>
      ) : null}
    </div>
  );
}
