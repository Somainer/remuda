import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { api } from "../../lib/api";
import { hubStore, useHub, useMainEntry, type MainEntry } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Instance } from "../../types/instance";
import type { Id, U64 } from "../../types/wire";
import { Composer } from "../session/Composer";
import { mainEcho, mainStatus, readMainTail, type MainEcho, type MainTail } from "./mainStatus";
import { useCompactLayout, useLineageSync } from "./useMain";
import css from "./mainCard.module.css";

/** Journal rows read for line 2 (same bounded window as the list phrases). */
const MAIN_TAIL = 64;
/**
 * The bounded journal tail behind line 2, re-read only when the chapter's
 * durableSeq moves (the 2 s instance poll carries it).
 */
function useMainTail(instance: Instance | null): { tail: MainTail | null; carried: string | null } {
  const [state, setState] = useState<{ key: string; tail: MainTail } | null>(null);
  // The newest reply seen on ANY chapter of this card: a fresh chapter has
  // no reply of its own yet, and the conversation is one across chapters.
  const [carried, setCarried] = useState<string | null>(null);
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
        if (cancelled) return;
        const tail = readMainTail(page.events);
        setState({ key, tail });
        if (tail.reply) setCarried(tail.reply.text);
      })
      .catch(() => {
        // Unreadable now: allow the next poll edge to try again.
        if (fetched.current === key) fetched.current = null;
      });
    return () => {
      cancelled = true;
    };
  }, [id, journalId, seq, key]);
  const tail = state && state.key.startsWith(`${id}@`) ? state.tail : null;
  return { tail, carried };
}

/** Everything the card and the 问 Main input read about the resolved Main. */
function useMainView(entry: MainEntry) {
  const hub = useHub();
  const nowMs = useNowTick();
  useLineageSync(entry);
  const instance = entry.instance;
  const lineage = hub.lineages[entry.lineageId] ?? null;
  const status = mainStatus({
    entry,
    lineage,
    host: hub.hosts.find((row) => row.id === instance.hostId),
    hostName: hubStore.hostName(instance.hostId),
    asking: hub.interactions.some((row) => row.instanceId === instance.id && row.state === "pending"),
    summary: hub.summaries[instance.id],
    nowMs,
  });
  const online = hub.connection === "live";
  return { hub, instance, status, online };
}

/**
 * Home Main card (D-061 ③, home.md §6.2; resolution D-057 §9): line 1 is
 * dot · Main · a status phrase, line 2 the first line of Main's last reply —
 * or the echo of this device's latest send. Renders nothing without an
 * address-owner lineage. The phone card carries no input (the 问 Main input
 * docks at the bottom of /m, {@link MainAskDock}); the desktop card holds the
 * inline input plus the read-only model·effort chip, the chapter and 展开 ›.
 */
export function MainCard({ placement }: { placement: "phone" | "desktop" }) {
  const entry = useMainEntry();
  if (!entry) return null;
  return <MainCardBody entry={entry} placement={placement} />;
}

function MainCardBody({ entry, placement }: { entry: MainEntry; placement: "phone" | "desktop" }) {
  const { hub, instance, status, online } = useMainView(entry);
  const { tail, carried } = useMainTail(instance);
  const echo = mainEcho({
    entry,
    bubbles: hub.bubbles,
    tail,
    carriedReply: carried,
    busy: status.phase === "working" || status.phase === "asking",
    online,
  });
  const [checking, setChecking] = useState(false);
  const desktop = placement === "desktop";
  const modelWord = instance.modelEffective?.id ?? instance.model ?? null;
  const effortWord = instance.effortEffective?.name ?? instance.effortName ?? null;

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
        </span>
      </Link>
      <EchoLine
        echo={echo}
        checking={checking}
        onRetry={(text) => void hubStore.send(instance.id as Id, text)}
        onReconcile={async (commandId) => {
          setChecking(true);
          try {
            await hubStore.reconcileSend(commandId as Id);
          } finally {
            setChecking(false);
          }
        }}
      />
      {desktop ? (
        <div className={css.meta} data-testid="main-card-meta">
          {modelWord || effortWord ? (
            <span className={css.modelChip} data-testid="main-card-model" title="只读：model · effort">
              {[modelWord, effortWord].filter(Boolean).join(" · ")}
            </span>
          ) : null}
          <span data-testid="main-card-chapter">第 {entry.generation} 章</span>
          <Link className={css.expand} to={`/s/${instance.id}`} data-testid="main-card-expand">
            展开 ›
          </Link>
        </div>
      ) : null}
      {desktop ? (
        <div className={css.ask} data-testid="main-ask">
          <AskMain entry={entry} />
        </div>
      ) : null}
    </section>
  );
}

/**
 * /m ⑨: the 问 Main input docked at the bottom of the home, directly above
 * PhoneNav — one bottom band (D-061). While it holds the soft keyboard the
 * nav steps aside (keyboardCompact.css) and the dock rides the band's bottom
 * edge; the list keeps its scroll position. Nothing renders without a Main.
 */
export function MainAskDock() {
  const entry = useMainEntry();
  if (!entry) return null;
  return (
    <div className={css.dock} data-testid="main-ask-dock">
      <AskMain entry={entry} />
    </div>
  );
}

/**
 * The 问 Main input: the compact Composer as one 56px row. Sends go to the
 * resolved chapter through the D-055 outbox; while Main is paused or stopped
 * the send button is 恢复; a host-offline / Hub-offline placeholder says the
 * message will queue.
 */
function AskMain({ entry }: { entry: MainEntry }) {
  const { instance, status, online } = useMainView(entry);
  const mobile = useCompactLayout();
  const [resuming, setResuming] = useState(false);
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
      : !online
        ? "离线 · 消息会排队，恢复连接后送达"
        : mobile
          ? "问 Main…"
          : "问 Main…  ⌘↵ 发送";
  return (
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
