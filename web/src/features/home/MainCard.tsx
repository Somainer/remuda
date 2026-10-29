import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { api } from "../../lib/api";
import { hubStore, useHub, useMainEntry, type MainEntry } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import { useWorkbenchViewport } from "../../lib/viewport";
import type { Instance } from "../../types/instance";
import type { Id, U64 } from "../../types/wire";
import { Composer } from "../session/Composer";
import { mainEcho, mainStatus, readMainTail, type MainEcho, type MainTail } from "./mainStatus";
import { useLineageSync } from "./useMain";
import css from "./mainCard.module.css";

/** Journal rows read for line 2 (same bounded window as the list phrases). */
const MAIN_TAIL = 64;
/**
 * The bounded journal tail behind line 2, re-read only when the chapter's
 * durableSeq moves (the 2 s instance poll carries it).
 */
function useMainTail(instance: Instance | null): MainTail | null {
  const [state, setState] = useState<{ key: string; tail: MainTail } | null>(null);
  const id = instance?.id ?? "";
  const journalId = instance?.journalId ?? "";
  const seq = String(instance?.durableSeq ?? "0");
  const key = `${id}@${seq}`;
  const fetched = useRef<string | null>(null);
  useEffect(() => {
    if (!id || !journalId || fetched.current === key) return;
    fetched.current = key;
    let cancelled = false;
    const from = Math.max(0, Number(seq) - MAIN_TAIL);
    api
      .eventsRead({ journalId: journalId as Id, afterSeq: String(from) as U64, limit: MAIN_TAIL })
      .then((page) => {
        if (!cancelled) setState({ key, tail: readMainTail(page.events) });
      })
      .catch(() => {
        // Unreadable now: allow the next poll edge to try again.
        if (fetched.current === key) fetched.current = null;
      });
    return () => {
      cancelled = true;
    };
  }, [id, journalId, seq, key]);
  if (!state || !state.key.startsWith(`${id}@`)) return null;
  return state.tail;
}

/**
 * Home Main card (D-057 §9 / H8a): line 1 is dot · Main · a status phrase,
 * line 2 the first line of Main's last reply — or the echo of this device's
 * latest send — and under it the 问 Main input. Renders nothing without an
 * address-owner lineage. The /m home and the desktop /sessions page mount the
 * same card; sends go to the resolved chapter through the D-055 outbox.
 */
export function MainCard({ placement }: { placement: "phone" | "desktop" }) {
  const entry = useMainEntry();
  if (!entry) return null;
  return <MainCardBody entry={entry} placement={placement} />;
}

function MainCardBody({ entry, placement }: { entry: MainEntry; placement: "phone" | "desktop" }) {
  const hub = useHub();
  const { mobile } = useWorkbenchViewport();
  const nowMs = useNowTick();
  useLineageSync(entry);
  const instance = entry.instance;
  const lineage = hub.lineages[entry.lineageId] ?? null;
  const host = hub.hosts.find((row) => row.id === instance.hostId);
  const hostName = hubStore.hostName(instance.hostId);
  const asking = hub.interactions.some((row) => row.instanceId === instance.id && row.state === "pending");
  const status = mainStatus({
    entry,
    lineage,
    host,
    hostName,
    asking,
    summary: hub.summaries[instance.id],
    nowMs,
  });
  const tail = useMainTail(instance);
  const online = hub.connection === "live";
  const echo = mainEcho({
    entry,
    bubbles: hub.bubbles,
    tail,
    busy: status.phase === "working" || status.phase === "asking",
    online,
  });
  const [resuming, setResuming] = useState(false);
  const [checking, setChecking] = useState(false);

  const sendText = async (text: string) => hubStore.send(instance.id as Id, text);

  const resume = async () => {
    setResuming(true);
    try {
      // The store refreshes the list on success; the resolution then lands on
      // the new chapter and the input turns back into 发送.
      await hubStore.resume(instance.id as Id, "structured");
    } finally {
      setResuming(false);
    }
  };

  const placeholder = status.resumable
    ? "Main 已暂停 · 点 恢复 继续"
    : status.hostOffline
      ? "主机离线 · 消息会排队，上线后送达"
      : online
        ? "问 Main…"
        : "离线 · 消息会排队，恢复连接后送达";

  return (
    <section
      className={css.card}
      data-testid="main-card"
      data-placement={placement}
      data-phase={status.phase}
      data-tone={status.tone}
      data-main-id={instance.id}
      aria-label="Main"
    >
      <Link to={`/s/${instance.id}`} className={css.head} data-testid="main-card-open">
        <span className={css.line1}>
          <StateDot status={status.dot} />
          <span className={css.name}>Main</span>
          <span className={css.sep} aria-hidden="true">
            ·
          </span>
          <span className={css.phrase} data-testid="main-card-status" title={status.phrase}>
            {status.phrase}
          </span>
          {placement === "desktop" && entry.generation > 1 ? (
            <span className={css.chapter} data-testid="main-card-chapter">
              第 {entry.generation} 章
            </span>
          ) : null}
        </span>
      </Link>
      <EchoLine
        echo={echo}
        checking={checking}
        onRetry={(text) => void sendText(text)}
        onReconcile={async (commandId) => {
          setChecking(true);
          try {
            await hubStore.reconcileSend(commandId as Id);
          } finally {
            setChecking(false);
          }
        }}
      />
      <div className={css.ask} data-testid="main-ask">
        <Composer
          key={instance.id}
          variant="ask"
          instanceId={instance.id}
          mobile={mobile}
          kind={instance.kind}
          phase="idle"
          capabilities={instance.capabilities}
          placeholder={placeholder}
          primaryOverride={
            status.resumable
              ? {
                  label: resuming ? "恢复中…" : "恢复",
                  testId: "main-ask-resume",
                  disabled: resuming,
                  title: "恢复 Main（续接下一章）",
                  onClick: () => void resume(),
                }
              : null
          }
          onSend={async (text, attachments, staged) => {
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
            return hubStore.send(instance.id as Id, text, attachments ?? [], previews);
          }}
        />
      </div>
    </section>
  );
}

function EchoLine({
  echo,
  checking,
  onRetry,
  onReconcile,
}: {
  echo: MainEcho;
  checking: boolean;
  onRetry: (text: string) => void;
  onReconcile: (commandId: string) => void;
}) {
  const text = useMemo(() => {
    switch (echo.kind) {
      case "empty":
        return "还没有回复";
      case "rejected":
        return echo.reason ? `${echo.text}：${echo.reason}` : echo.text;
      default:
        return echo.text;
    }
  }, [echo]);
  return (
    <div className={css.line2} data-testid="main-card-reply" data-kind={echo.kind} role={echo.kind === "rejected" ? "alert" : undefined}>
      <span className={css.replyText} title={text}>
        {text}
      </span>
      {echo.kind === "rejected" ? (
        <button
          type="button"
          className={css.action}
          data-testid={echo.superseded ? "main-card-send-current" : "main-card-retry"}
          // A NEW send with a new commandId (D-055: never an automatic resend
          // under a new id — this is the owner's explicit choice).
          onClick={() => onRetry(echo.bubble.text)}
        >
          {echo.superseded ? "发送给当前 Main" : "重试"}
        </button>
      ) : null}
      {echo.kind === "unknown" && echo.bubble.commandId ? (
        <button
          type="button"
          className={css.action}
          data-testid="main-card-reconcile"
          disabled={checking}
          // One GET of the command row; never a resend.
          onClick={() => onReconcile(echo.bubble.commandId!)}
        >
          {checking ? "核对中…" : "核对"}
        </button>
      ) : null}
    </div>
  );
}
