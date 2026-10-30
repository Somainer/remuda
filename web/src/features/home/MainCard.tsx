import { useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { readDraft, writeDraft } from "../../lib/drafts";
import type { AttachmentRef } from "../../lib/attachments";
import { hubStore, useHub, useMainEntry, type LocalBubble, type MainEntry } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Id } from "../../types/wire";
import { Composer } from "../session/Composer";
import { mainEcho, mainStatus, type MainEcho } from "./mainStatus";
import { useMainTail } from "./mainTail";
import { useCompactLayout, useLineageSync } from "./useMain";
import css from "./mainCard.module.css";

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
  const { tail, carried, read } = useMainTail(entry, hub.instances);
  const echo = mainEcho({
    entry,
    bubbles: hub.bubbles,
    tail,
    tailRead: read,
    carriedReply: carried,
    busy: status.phase === "working" || status.phase === "asking",
    online,
  });
  const [checking, setChecking] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const retryingRef = useRef(false);
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
        retrying={retrying}
        onRetry={(bubble) => {
          // One send per tap: a double tap never sends twice (r2 item 8).
          if (retryingRef.current) return;
          retryingRef.current = true;
          setRetrying(true);
          void resend(instance.id as Id, bubble).finally(() => {
            retryingRef.current = false;
            setRetrying(false);
          });
        }}
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
 * resolved chapter through the D-055 outbox; while Main is paused or ended
 * the send button is 恢复; a host-offline / Hub-offline placeholder says the
 * message will queue.
 */
function AskMain({ entry }: { entry: MainEntry }) {
  const { instance, status, online } = useMainView(entry);
  const mobile = useCompactLayout();
  // The draft and staged files belong to the LINEAGE: Resume or a restart
  // moves Main to a new chapter and the half-typed message must survive it.
  const scope = askScope(entry.lineageId);
  useState(() => adoptChapterDrafts(scope, entry));
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
    ? status.phase === "paused"
      ? "Main 已暂停 · 点 恢复 继续"
      : "Main 已结束 · 点 恢复 继续"
    : status.hostOffline
      ? "主机离线 · 消息会排队，上线后送达"
      : !online
        ? "离线 · 消息会排队，恢复连接后送达"
        : mobile
          ? "问 Main…"
          : "问 Main…  ⌘↵ 发送";
  return (
    <Composer
      key={scope}
      variant="ask"
      instanceId={instance.id}
      draftScope={scope}
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

/**
 * 重试 / 发送给当前 Main: a NEW send of the rejected message — its text and
 * the same attachment refs and previews. Only reached for a message whose
 * attachments are valid for `target` (same chapter, or none).
 */
function resend(target: Id, bubble: LocalBubble): Promise<boolean> {
  const previews = bubble.attachments ?? [];
  const refs: AttachmentRef[] = previews.map((item, position) => ({
    index: item.index ?? position + 1,
    objectId: item.objectId,
    kind: item.kind,
    mediaType: item.mediaType,
    name: item.name,
    size: item.size,
  }));
  return hubStore.send(target, bubble.text, refs, previews);
}

/** Draft scope of the 问 Main input: one per Main lineage. */
function askScope(lineageId: string): string {
  return `main-ask:${lineageId}`;
}

/**
 * Move a draft left under an earlier chapter's id (r1 kept the input per
 * chapter; an ended chapter's own page has no composer) into the lineage
 * draft, once, before the input reads it. The current chapter's draft stays:
 * it belongs to that chapter's session page.
 */
function adoptChapterDrafts(scope: string, entry: MainEntry) {
  if (readDraft(scope)) return;
  const earlier = entry.chapterIds.filter((id) => id !== entry.instance.id).reverse();
  for (const id of earlier) {
    const draft = readDraft(id);
    if (!draft) continue;
    writeDraft(scope, draft);
    writeDraft(id, "");
    return;
  }
}

function EchoLine({
  echo,
  checking,
  retrying,
  onRetry,
  onReconcile,
}: {
  echo: MainEcho;
  checking: boolean;
  /** A 重试 / 发送给当前 Main send is in flight. */
  retrying: boolean;
  onRetry: (bubble: LocalBubble) => void;
  onReconcile: (commandId: string) => void;
}) {
  // Hub objects are bound to the chapter they were uploaded under, so a
  // message with attachments cannot move to another chapter (r2 item 6).
  const stranded = echo.kind === "rejected" && echo.superseded && (echo.bubble.attachments?.length ?? 0) > 0;
  const text = useMemo(() => {
    switch (echo.kind) {
      case "empty":
        return "还没有回复";
      case "unread":
        // Not read yet, or unreadable right now: a neutral mark, never the
        // claim that Main has not replied.
        return "—";
      case "rejected": {
        const line = echo.reason ? `${echo.text}：${echo.reason}` : echo.text;
        return stranded ? `${line} · 含附件，请在当前 Main 重新添加` : line;
      }
      default:
        return echo.text;
    }
  }, [echo, stranded]);
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
          disabled={stranded || retrying}
          title={stranded ? "附件属于原来的章节，无法转发；请在当前 Main 重新添加" : undefined}
          // A NEW send with a new commandId (D-055: never an automatic resend
          // under a new id — this is the owner's explicit choice).
          onClick={() => onRetry(echo.bubble)}
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
