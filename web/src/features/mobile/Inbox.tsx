import { useEffect, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import {
  deriveInboxRows,
  derivePushBanner,
  INBOX_KINDS,
  KIND_SEGMENT_LABEL,
  parseKindParam,
  type InboxInteractionRow,
  type InboxInstanceRow,
  type PushBannerState,
} from "./inboxRows";
import { hubStore, useHub } from "../../lib/store";
import { readPushStatus, subscribePush, type PushStatus } from "../../lib/push";
import { thisDeviceId } from "../../lib/interactionStatus";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import css from "./inbox.module.css";

const BANNER_DISMISS_KEY = "runtime.m-inbox-push-banner-dismissed";

function bannerDismissed(): boolean {
  try {
    return localStorage.getItem(BANNER_DISMISS_KEY) === "1";
  } catch {
    return false;
  }
}

function dismissBanner() {
  try {
    localStorage.setItem(BANNER_DISMISS_KEY, "1");
  } catch {
    /* private mode: dismissal simply will not persist */
  }
}

/** Context remaining ring. Null pct renders nothing — never a fake 0 (§3.3). */
function ContextRing({ pct }: { pct: number | null }) {
  if (pct == null) return null;
  const clamped = Math.max(0, Math.min(100, pct));
  const r = 11;
  const circ = 2 * Math.PI * r;
  return (
    <span className={css.ring} title={`上下文剩余 ${clamped}%`} aria-hidden>
      <svg viewBox="0 0 28 28" width="28" height="28">
        <circle className={css.ringTrack} cx="14" cy="14" r={r} />
        <circle
          className={css.ringFill}
          cx="14"
          cy="14"
          r={r}
          strokeDasharray={circ}
          strokeDashoffset={circ * (1 - clamped / 100)}
        />
      </svg>
      <span className={css.ringText}>{clamped}%</span>
    </span>
  );
}

function MetaLine({
  host,
  workspace,
  harness,
  time,
}: {
  host: string;
  workspace: string;
  harness: string;
  time: string;
}) {
  return (
    <div className={css.meta}>
      <span>{host}</span>
      <span className={css.sep}>/</span>
      <span>{workspace || "—"}</span>
      <span className={css.sep}>·</span>
      <span>{harness}</span>
      <span className={css.sep}>·</span>
      <span>{time}</span>
    </div>
  );
}

function InteractionActions({
  row,
  item,
  onRespond,
}: {
  row: InboxInteractionRow;
  item: Interaction | undefined;
  onRespond: (item: Interaction, answer: InteractionAnswer) => void;
}) {
  if (row.goAnswer) {
    return (
      <div className={css.actions}>
        <Link
          to={`/s/${row.instanceId}`}
          className={`${css.btn} ${css.btnPrimary}`}
          data-testid="m-inbox-answer"
        >
          去回答
        </Link>
      </div>
    );
  }
  if (!item) return null;

  const busy = row.uiState === "answering";
  // Paused and not-yet-answerable rows may not submit. The answering lock is
  // the store's: buttons stay disabled until the journal receipt clears
  // hub.answering (ui-spec §2.5). No local lock is taken here.
  const disabled = row.uiState !== "pending" || !row.answerable;

  const optionButtons =
    item.request.kind === "approval" || item.request.kind === "plan-review"
      ? row.options.map((opt) => (
          <button
            key={opt.id}
            type="button"
            className={`${css.btn} ${opt.effect === "deny" ? css.btnDeny : css.btnPrimary}`}
            disabled={disabled}
            data-testid={`m-inbox-option-${opt.id}`}
            onClick={() => {
              if (item.request.kind !== "approval" && item.request.kind !== "plan-review") return;
              if (item.request.kind === "approval") {
                onRespond(item, {
                  kind: "approval",
                  optionId: opt.id,
                  inputDigest: item.request.inputDigest,
                });
              } else {
                onRespond(item, {
                  kind: "plan-review",
                  optionId: opt.id,
                  planRevision: item.request.planRevision,
                  planDigest: item.request.planDigest,
                  feedback: null,
                });
              }
            }}
          >
            {opt.label}
          </button>
        ))
      : null;

  return (
    <div className={css.actions}>
      {busy ? (
        <span className={`${css.btn} ${css.btnBusy}`} data-testid="m-inbox-submitting">
          <span className={`${css.spin} spin`} />
          已提交
        </span>
      ) : (
        optionButtons
      )}
      <Link to={`/s/${row.instanceId}`} className={`${css.btn} ${css.btnMute}`}>
        打开会话
      </Link>
    </div>
  );
}

function InteractionRowCard({
  row,
  item,
  onRespond,
}: {
  row: InboxInteractionRow;
  item: Interaction | undefined;
  onRespond: (item: Interaction, answer: InteractionAnswer) => void;
}) {
  return (
    <article
      ref={(el) => {
        // Focus scroll looks the element up by interaction id; React 19 ref
        // cleanup keeps the map from keeping detached rows.
        rowRefs.set(row.interactionId, el);
        return () => {
          rowRefs.delete(row.interactionId);
        };
      }}
      className={`${css.row} ${row.focused ? css.rowFocus : ""}`}
      data-testid="m-inbox-row"
      data-interaction-id={row.interactionId}
      data-kind={row.interactionKind}
      data-state={row.uiState}
      data-focus={row.focused ? "true" : "false"}
    >
      <div className={css.rowHead}>
        <StateDot
          status={row.uiState === "paused" ? "unknown" : "blocked"}
          title={row.uiState === "paused" ? "主机离线，交互暂停" : "待处理"}
        />
        <span className={css.headline} title={row.headline}>
          {row.headline}
        </span>
        <span className={css.rowSpacer} />
        <ContextRing pct={row.contextPct} />
      </div>
      {row.subtitle ? (
        <p
          className={`${css.subtitle} ${row.uiState === "paused" ? css.subtitleMute : ""} ${
            row.subtitle && item?.carrier === "native-tty" ? css.subtitlePre : ""
          }`}
          title={row.subtitle}
        >
          {row.subtitle}
        </p>
      ) : null}
      {item && item.carrier === "harness-hook" ? (
        <p className={css.note}>来自工具钩子 · 回答直接决定工具是否执行</p>
      ) : null}
      {item && !item.answerable ? <p className={css.note}>请打开会话查看完整终端提示</p> : null}
      {row.uiState === "paused" ? (
        <p className={css.pausedNote} data-testid="m-inbox-paused">
          主机离线，交互暂停
        </p>
      ) : null}
      <MetaLine host={row.hostLabel} workspace={row.workspaceLabel} harness={row.harness} time={row.timeLabel} />
      <InteractionActions row={row} item={item} onRespond={onRespond} />
    </article>
  );
}

// Module-level: the focus effect below needs the mounted row element without
// threading a ref callback through every row. Entries are overwritten on
// re-render and deleted on unmount, so the map never grows past the rendered
// tier.
const rowRefs = new Map<string, HTMLElement | null>();

function RecentRowCard({ row }: { row: InboxInstanceRow }) {
  return (
    <article
      className={css.row}
      data-testid="m-inbox-recent-row"
      data-instance-id={row.instanceId}
      data-status={row.status}
    >
      <div className={css.rowHead}>
        <StateDot status={row.status} />
        <Link to={`/s/${row.instanceId}`} className={css.headlineLink} title={row.title}>
          {row.title}
        </Link>
        <span className={css.rowSpacer} />
        <ContextRing pct={row.contextPct} />
      </div>
      {row.subtitle ? (
        <p
          className={`${css.subtitle} ${row.status === "exited" ? css.subtitleError : ""}`}
          title={row.subtitle}
        >
          {row.subtitle}
        </p>
      ) : null}
      <MetaLine host={row.hostLabel} workspace={row.workspaceLabel} harness={row.harness} time={row.timeLabel} />
    </article>
  );
}

export function Inbox() {
  const hub = useHub();
  const [params, setParams] = useSearchParams();
  const focus = params.get("focus");
  const kind = parseKindParam(params.get("kind"));
  const deviceId = useMemo(() => thisDeviceId(), []);

  const [push, setPush] = useState<PushStatus | null>(null);
  const [dismissed, setDismissed] = useState(bannerDismissed);
  const [showHomeHint, setShowHomeHint] = useState(false);

  // Read the push state on mount ONLY. Permission is never requested here —
  // D-049 §6: requests happen on the 开启 tap (or in settings), never on load.
  useEffect(() => {
    let live = true;
    void readPushStatus().then((status) => {
      if (live) setPush(status);
    });
    return () => {
      live = false;
    };
  }, []);

  const rows = useMemo(
    () =>
      deriveInboxRows(
        {
          interactions: hub.interactions,
          instances: hub.instances,
          hosts: hub.hosts,
          answering: hub.answering,
          phrases: hub.summaries,
          rollups: hub.usageRollup,
          deviceId,
          titleOf: (id) => hubStore.titleOf(id),
          hostName: (id) => hubStore.hostName(id),
          workspaceLabel: (id) => hubStore.workspaceOf(id)?.label ?? "",
        },
        { kind, focus },
      ),
    [hub, kind, focus, deviceId],
  );
  const interactionsById = useMemo(
    () => new Map(hub.interactions.map((item) => [item.id, item])),
    [hub.interactions],
  );

  // Project live phrases for every instance the tiers render, the same way
  // SessionList does: scoped to this mounted screen, coalesced in the store,
  // skipped while durableSeq is unchanged. refresh() deliberately does not
  // poll journals, so without this recent rows would never gain text.
  const phraseIds = useMemo(() => {
    const ids = new Set(rows.recent.map((row) => row.instanceId));
    for (const row of rows.pending) ids.add(row.instanceId);
    return Array.from(ids);
  }, [rows]);
  useEffect(() => {
    if (!phraseIds.length) return;
    const tick = () => void hubStore.hydrateRowSummaries(phraseIds);
    tick();
    const timer = window.setInterval(tick, 2500);
    return () => window.clearInterval(timer);
  }, [phraseIds.join(",")]); // eslint-disable-line react-hooks/exhaustive-deps

  // Deep link (?focus=<interactionId>, ui-spec §1.2): highlight the row and
  // scroll it into view the moment it renders. The redirect layer carries
  // the query from /approvals verbatim.
  const focusedKey = rows.pending.find((row) => row.focused)?.interactionId ?? null;
  useEffect(() => {
    if (!focus) return;
    const el = rowRefs.get(focus);
    el?.scrollIntoView({ block: "center", behavior: "auto" });
  }, [focus, focusedKey]);

  useEffect(() => {
    return () => {
      rowRefs.clear();
    };
  }, []);

  const respond = (item: Interaction, answer: InteractionAnswer) => {
    void hubStore.respond(item.id, answer);
  };

  const setKind = (next: (typeof INBOX_KINDS)[number]) => {
    const search = new URLSearchParams(params);
    if (next === "all") search.delete("kind");
    else search.set("kind", next);
    setParams(search);
  };

  const banner: PushBannerState = dismissed ? { show: false } : derivePushBanner(push);

  const enablePush = async () => {
    const result = await subscribePush();
    setPush(await readPushStatus());
    if (result.ok) {
      setShowHomeHint(false);
    } else if (result.reason === "denied") {
      setShowHomeHint(true);
    }
  };

  return (
    <div className={css.page} data-testid="m-inbox">
      <header className={css.top}>
        <div className={css.titleRow}>
          <h1 className={css.title}>收件箱</h1>
          <span className={css.pending} data-testid="m-inbox-pending-count">
            待处理 {rows.pending.length}
          </span>
        </div>
        <div className={css.seg} role="tablist" aria-label="交互类型">
          {INBOX_KINDS.map((id) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={kind === id}
              data-testid={`m-inbox-kind-${id}`}
              className={`${css.segBtn} ${kind === id ? css.segOn : ""}`}
              onClick={() => setKind(id)}
            >
              {KIND_SEGMENT_LABEL[id]}
            </button>
          ))}
        </div>
      </header>

      {banner.show ? (
        <div className={css.banner} data-testid="m-inbox-push-banner" data-mode={banner.mode}>
          <span className={css.bannerText}>
            {banner.mode === "homescreen"
              ? "加到主屏幕后，收件箱才能在后台提醒你"
              : "开启通知后，收件箱才能在后台提醒你"}
          </span>
          {banner.mode === "homescreen" ? (
            <button
              type="button"
              className={css.btnPrimary}
              data-testid="m-inbox-push-enable"
              onClick={() => setShowHomeHint((v) => !v)}
            >
              先加到主屏幕
            </button>
          ) : (
            <button
              type="button"
              className={css.btnPrimary}
              data-testid="m-inbox-push-enable"
              onClick={() => void enablePush()}
            >
              开启
            </button>
          )}
          <button
            type="button"
            className={css.bannerClose}
            data-testid="m-inbox-push-dismiss"
            aria-label="关闭通知提示"
            onClick={() => {
              dismissBanner();
              setDismissed(true);
            }}
          >
            ×
          </button>
        </div>
      ) : null}
      {showHomeHint && banner.show ? (
        <p className={css.bannerHint} data-testid="m-inbox-push-hint">
          用 Safari 底部分享菜单「加到主屏幕」，再回来开启通知。
        </p>
      ) : null}

      <section className={css.tier}>
        <h2 className={css.tierTitle} data-testid="m-inbox-tier-pending">
          待你处理 ({rows.pending.length})
        </h2>
        {rows.pending.map((row) => (
          <InteractionRowCard
            key={row.interactionId}
            row={row}
            item={interactionsById.get(row.interactionId)}
            onRespond={respond}
          />
        ))}
        {rows.pending.length === 0 ? <p className={css.tierEmpty}>这里没有等你处理的交互</p> : null}
      </section>

      <section className={css.tier}>
        <h2 className={css.tierTitle} data-testid="m-inbox-tier-recent">
          进行中 · 最近 ({rows.recent.length})
        </h2>
        {rows.recent.map((row) => (
          <RecentRowCard key={row.instanceId} row={row} />
        ))}
      </section>
    </div>
  );
}
