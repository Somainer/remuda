import {
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import { Link, useSearchParams } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { PageHeader } from "../../components/PageHeader";
import { hubStore, useHub } from "../../lib/store";
import { useIncrementalLimit } from "../../lib/useIncrementalLimit";
import { formatClock } from "../../lib/format";
import { profileRegion } from "../../lib/profileFlags";
import { thisDeviceId } from "../../lib/interactionStatus";
import { readPushStatus, subscribePush, type PushStatus } from "../../lib/push";
import type { Interaction, InteractionAnswer } from "../../types/interaction";
import ui from "../../styles/ui.module.css";
import desktopCss from "../../pages/ApprovalsPage.module.css";
import {
  DEPARTED_STATUS_TEXT,
  decisionPreview,
  decisionTitle,
  deriveApprovalRows,
  type ApprovalRow,
} from "./approvalRows";
import { DecisionCard, type DecisionView, type InboxMode } from "./ApprovalCard";
import {
  contextRingLabel,
  deriveInboxRows,
  derivePushBanner,
  INBOX_KINDS,
  KIND_SEGMENT_LABEL,
  parseKindParam,
  type InboxInstanceRow,
  type InboxInteractionRow,
  type PushBannerState,
} from "../mobile/inboxRows";
import compactCss from "../mobile/inbox.module.css";

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

type RespondFn = (item: Interaction, answer: InteractionAnswer) => void | Promise<void>;

/* ------------------------------------------------------------------ */
/* Kind filter: one radiogroup primitive for both routes (ui-spec     */
/* §2.5). role=radiogroup + roving tabindex; the .segItem control is   */
/* 26px tall on fine pointers and reaches 44px on coarse ones.         */
/* ------------------------------------------------------------------ */

const KIND_OPTIONS = INBOX_KINDS.map((id) => ({ id, label: KIND_SEGMENT_LABEL[id] }));

function KindSegment({
  value,
  onChange,
  trackClassName,
}: {
  value: string;
  onChange: (id: (typeof INBOX_KINDS)[number]) => void;
  trackClassName: string;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const forward = event.key === "ArrowRight" || event.key === "ArrowDown";
    const back = event.key === "ArrowLeft" || event.key === "ArrowUp";
    if (!forward && !back) return;
    event.preventDefault();
    const next = (index + (forward ? 1 : -1) + KIND_OPTIONS.length) % KIND_OPTIONS.length;
    refs.current[next]?.focus();
    const option = KIND_OPTIONS[next];
    if (option) onChange(option.id);
  };
  return (
    <div className={trackClassName} role="radiogroup" aria-label="交互类型" data-testid="inbox-kind">
      {KIND_OPTIONS.map((option, index) => (
        <button
          key={option.id}
          ref={(el) => {
            refs.current[index] = el;
          }}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          tabIndex={value === option.id ? 0 : -1}
          className={ui.segItem}
          data-testid={`inbox-kind-${option.id}`}
          onClick={() => onChange(option.id)}
          onKeyDown={(event) => onKeyDown(event, index)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

/* ------------------------------------------------------------------ */
/* Adapters: each mode's pure derivation feeds the SAME DecisionView.  */
/* ------------------------------------------------------------------ */

type ContextFields = Pick<
  DecisionView,
  "timeLabel" | "hostLabel" | "workspaceLabel" | "instanceKind"
>;

function desktopView(row: ApprovalRow, workspaceLabel: string): DecisionView {
  const { item, instance } = row;
  const fields: ContextFields = {
    timeLabel: formatClock(item.createdAt),
    hostLabel: hubStore.hostName(item.hostId),
    workspaceLabel,
    instanceKind: instance?.kind ?? "—",
  };
  return {
    key: item.id,
    sig: row.sig,
    item,
    uiState: row.uiState as DecisionView["uiState"],
    focused: row.focused,
    ...fields,
  };
}

function compactView(
  row: InboxInteractionRow,
  item: Interaction,
): DecisionView {
  return {
    key: row.interactionId,
    sig: row.sig,
    item,
    uiState: row.uiState,
    focused: row.focused,
    timeLabel: row.timeLabel,
    hostLabel: row.hostLabel,
    workspaceLabel: row.workspaceLabel,
    instanceKind: row.harness,
  };
}

/* ------------------------------------------------------------------ */
/* Desktop-only slim 已离队 row (third tier, no actions).              */
/* ------------------------------------------------------------------ */

export type DepartedView = DecisionView & {
  stateText: string;
  previewText: string;
  title: string;
};

export const DepartedRow = memo(
  function DepartedRow({
    view,
    onRowRender,
  }: {
    view: DepartedView;
    /**
     * Commit probe INSIDE the memo boundary: fired after every render that
     * actually commits. A memo bail-out (equal sig) runs neither the function
     * nor this effect, so tests can count real commits. The production shell
     * never passes it (zero work on that path).
     */
    onRowRender?: (interactionId: string) => void;
  }) {
    useEffect(() => {
      onRowRender?.(view.item.id);
    });
    return (
    <article
      className={desktopCss.departedRow}
      data-testid="approval-row"
      data-interaction-id={view.item.id}
      data-state={view.item.state}
    >
      <div className={desktopCss.departedMark} aria-hidden>
        ○
      </div>
      <div className={desktopCss.departedMeta}>
        {view.timeLabel} · {view.hostLabel}
        {view.workspaceLabel ? ` / ${view.workspaceLabel}` : ""}
      </div>
      <div className={desktopCss.departedTitle}>
        {view.title} · {view.previewText}
      </div>
      <div className={desktopCss.departedState}>{view.stateText}</div>
    </article>
  );
}, areDepartedEqual);

/**
 * The parent builds a fresh `view` object every 2 s poll. Bail out when its
 * sig is unchanged. The sig (approvalRows.rowSignature) already covers every
 * field this row renders — createdAt (time), host label/state, workspace
 * label, instance kind, uiState (stateText) and the request title/preview —
 * so an equal sig means an identical DOM and the row skips the commit.
 */
function areDepartedEqual(
  prev: { view: { sig: string } },
  next: { view: { sig: string } },
): boolean {
  return prev.view.sig === next.view.sig;
}

/**
 * The real keyed 已离队 list. A 2 s poll re-derives fresh ApprovalRow/view
 * objects; keys keep React identity stable and DepartedRow's sig comparator
 * skips rows whose content did not change, so unchanged departed rows do not
 * commit when a new interaction arrives.
 */
export function DepartedList({
  rows,
  workspaceLabel,
  onRowRender,
}: {
  rows: ApprovalRow[];
  workspaceLabel: (workspaceId: string) => string;
  onRowRender?: (interactionId: string) => void;
}) {
  if (rows.length === 0) return null;
  return (
    <div className={desktopCss.departed} data-testid="inbox-departed">
      <div className={desktopCss.departedLabel}>已离队</div>
      {rows.map((row) => {
        const base = desktopView(row, workspaceLabel(row.instance?.workspaceId ?? ""));
        return (
          <DepartedRow
            key={row.item.id}
            view={{
              ...base,
              stateText: DEPARTED_STATUS_TEXT[row.uiState as "expired" | "superseded"],
              title: decisionTitle(row.item),
              previewText: decisionPreview(row.item),
            }}
            onRowRender={onRowRender}
          />
        );
      })}
    </div>
  );
}

/* ------------------------------------------------------------------ */
/* Compact-only recent-instance row (进行中 · 最近).                   */
/* ------------------------------------------------------------------ */

function ContextRing({ pct }: { pct: number | null }) {
  if (pct == null) return null;
  const clamped = Math.max(0, Math.min(100, pct));
  const r = 11;
  const circ = 2 * Math.PI * r;
  const label = contextRingLabel(clamped);
  return (
    <span className={compactCss.ring} title={label ?? undefined} role="img" aria-label={label ?? ""}>
      <svg viewBox="0 0 28 28" width="28" height="28" aria-hidden="true">
        <circle className={compactCss.ringTrack} cx="14" cy="14" r={r} />
        <circle
          className={compactCss.ringFill}
          cx="14"
          cy="14"
          r={r}
          strokeDasharray={circ}
          strokeDashoffset={circ * (1 - clamped / 100)}
        />
      </svg>
      <span className={compactCss.ringText} aria-hidden="true">
        {clamped}%
      </span>
    </span>
  );
}

function MetaLine({ host, workspace, harness, time }: { host: string; workspace: string; harness: string; time: string }) {
  return (
    <div className={compactCss.meta}>
      <span>{host}</span>
      <span className={compactCss.sep}>/</span>
      <span>{workspace || "—"}</span>
      <span className={compactCss.sep}>·</span>
      <span>{harness}</span>
      <span className={compactCss.sep}>·</span>
      <span>{time}</span>
    </div>
  );
}

const RecentRowCard = memo(
  function RecentRowCard({ row }: { row: InboxInstanceRow }) {
    return (
      <article
        className={compactCss.row}
        data-testid="m-inbox-recent-row"
        data-instance-id={row.instanceId}
        data-status={row.status}
      >
        <div className={compactCss.rowHead}>
          <StateDot status={row.status} />
          <Link to={`/s/${row.instanceId}`} className={compactCss.headlineLink} title={row.title}>
            {row.title}
          </Link>
          <span className={compactCss.rowSpacer} />
          <ContextRing pct={row.contextPct} />
        </div>
        {row.subtitle ? (
          <p
            className={`${compactCss.subtitle} ${row.status === "exited" ? compactCss.subtitleError : ""}`}
            title={row.subtitle}
          >
            {row.subtitle}
          </p>
        ) : null}
        <MetaLine host={row.hostLabel} workspace={row.workspaceLabel} harness={row.harness} time={row.timeLabel} />
      </article>
    );
  },
  (prev, next) => prev.row.sig === next.row.sig,
);

/* ------------------------------------------------------------------ */
/* Shell                                                               */
/* ------------------------------------------------------------------ */

export function InboxShell({ mode }: { mode: InboxMode }) {
  const compact = mode === "compact";
  const hub = useHub();
  const [params, setParams] = useSearchParams();
  const focus = params.get("focus");
  // Same ?kind= vocabulary on both routes; parseKindParam validates it.
  const kind = parseKindParam(params.get("kind"));
  const hostFilter = params.get("host") ?? "";
  const workspaceFilter = params.get("workspace") ?? "";
  const deviceId = useMemo(() => thisDeviceId(), []);

  const workspaceById = useMemo(
    () => new Map(hub.workspaces.map((workspace) => [workspace.id, workspace])),
    [hub.workspaces],
  );
  const workspaceLabel = useCallback(
    (id: string) => workspaceById.get(id)?.label ?? "",
    [workspaceById],
  );

  const respond = useCallback<RespondFn>((item, answer) => hubStore.respond(item.id, answer), []);

  const setKind = (id: (typeof INBOX_KINDS)[number]) => {
    const next = new URLSearchParams(params);
    if (id === "all") next.delete("kind");
    else next.set("kind", id);
    setParams(next);
  };

  const toggleFilter = (key: "host" | "workspace", value: string) => {
    const next = new URLSearchParams(params);
    if (params.get(key) === value) next.delete(key);
    else next.set(key, value);
    setParams(next);
  };

  if (compact) return <CompactInbox
    hub={hub}
    kind={kind}
    focus={focus}
    deviceId={deviceId}
    workspaceLabel={workspaceLabel}
    onSetKind={setKind}
    respond={respond}
  />;

  return (
    <DesktopInbox
      hub={hub}
      kind={kind}
      focus={focus}
      hostFilter={hostFilter}
      workspaceFilter={workspaceFilter}
      deviceId={deviceId}
      workspaceLabel={workspaceLabel}
      onSetKind={setKind}
      onToggleFilter={toggleFilter}
      respond={respond}
    />
  );
}

/* ------------------------------------------------------------------ */
/* Desktop                                                             */
/* ------------------------------------------------------------------ */

type Hub = ReturnType<typeof useHub>;

function DesktopInbox({
  hub,
  kind,
  focus,
  hostFilter,
  workspaceFilter,
  deviceId,
  workspaceLabel,
  onSetKind,
  onToggleFilter,
  respond,
}: {
  hub: Hub;
  kind: string;
  focus: string | null;
  hostFilter: string;
  workspaceFilter: string;
  deviceId: string;
  workspaceLabel: (id: string) => string;
  onSetKind: (id: (typeof INBOX_KINDS)[number]) => void;
  onToggleFilter: (key: "host" | "workspace", value: string) => void;
  respond: RespondFn;
}) {
  const rows = useMemo(
    () =>
      profileRegion("approvals.deriveRows", () =>
        deriveApprovalRows(
          {
            interactions: hub.interactions,
            instances: hub.instances,
            hosts: hub.hosts,
            answering: hub.answering,
            deviceId,
            workspaceLabel,
          },
          { kind, hostId: hostFilter, workspaceId: workspaceFilter, focus },
        ),
      ),
    [
      hub.interactions,
      hub.instances,
      hub.hosts,
      hub.answering,
      deviceId,
      workspaceLabel,
      kind,
      hostFilter,
      workspaceFilter,
      focus,
    ],
  );

  const { queue, departed } = rows;
  const pendingCount = queue.length;
  const filterKey = `${kind}\u0000${hostFilter}\u0000${workspaceFilter}`;
  const queueLimit = useIncrementalLimit(queue.length, { resetKey: filterKey });
  const departedLimit = useIncrementalLimit(departed.length, { resetKey: filterKey });

  return (
    <div className={desktopCss.page} data-testid="approvals-page">
      <PageHeader
        title="收件箱"
        actions={
          <span className={desktopCss.pending}>
            <span className={desktopCss.pendingDot} aria-hidden />
            待处理 {pendingCount}
          </span>
        }
      />
      <div className={desktopCss.body}>
        <div className={desktopCss.controls}>
          <KindSegment value={kind} onChange={onSetKind} trackClassName={ui.seg} />
          <div className={desktopCss.filters}>
            {hub.hosts.map((host) => (
              <button
                key={host.id}
                type="button"
                className={`${ui.chip} ${hostFilter === host.id ? ui.chipOn : ""}`}
                aria-pressed={hostFilter === host.id}
                onClick={() => onToggleFilter("host", host.id)}
              >
                {host.label}
              </button>
            ))}
            {hub.workspaces.map((workspace) => (
              <button
                key={workspace.id}
                type="button"
                className={`${ui.chip} ${workspaceFilter === workspace.id ? ui.chipOn : ""}`}
                aria-pressed={workspaceFilter === workspace.id}
                onClick={() => onToggleFilter("workspace", workspace.id)}
              >
                {workspace.label}
              </button>
            ))}
          </div>
        </div>

        <div className={desktopCss.queue}>
          {queue.slice(0, queueLimit).map((row) => (
            <DecisionCard
              key={row.item.id}
              view={desktopView(row, workspaceLabel(row.instance?.workspaceId ?? ""))}
              mode="desktop"
              onRespond={respond}
            />
          ))}
          {departed.length ? (
            <DepartedList rows={departed.slice(0, departedLimit)} workspaceLabel={workspaceLabel} />
          ) : null}
          {queue.length + departed.length === 0 ? <p className={desktopCss.empty}>没有待处理交互</p> : null}
        </div>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ */
/* Compact                                                             */
/* ------------------------------------------------------------------ */

function CompactInbox({
  hub,
  kind,
  focus,
  deviceId,
  workspaceLabel,
  onSetKind,
  respond,
}: {
  hub: Hub;
  kind: (typeof INBOX_KINDS)[number];
  focus: string | null;
  deviceId: string;
  workspaceLabel: (id: string) => string;
  onSetKind: (id: (typeof INBOX_KINDS)[number]) => void;
  respond: RespondFn;
}) {
  const [push, setPush] = useState<PushStatus | null>(null);
  const [dismissed, setDismissed] = useState(bannerDismissed);
  const [showHomeHint, setShowHomeHint] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);

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
          workspaceLabel,
        },
        { kind, focus },
      ),
    [
      hub.interactions,
      hub.instances,
      hub.hosts,
      hub.answering,
      hub.summaries,
      hub.usageRollup,
      deviceId,
      workspaceLabel,
      kind,
      focus,
    ],
  );

  const interactionsById = useMemo(
    () => new Map(hub.interactions.map((item) => [item.id, item])),
    [hub.interactions],
  );

  // Project live phrases for every instance the tiers render (same cadence as
  // the prior compact inbox): refresh() never polls journals.
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

  const pendingLimit = useIncrementalLimit(rows.pending.length, { resetKey: kind });
  const recentLimit = useIncrementalLimit(rows.recent.length, { resetKey: kind });

  // Deep link (?focus=): the row mounts over rAF slices, so re-run as the
  // pending limit grows until the element is in the viewport.
  const focusedKey = rows.pending.find((row) => row.focused)?.interactionId ?? null;
  useEffect(() => {
    if (!focus || !focusedKey) return;
    const el = scrollRef.current?.querySelector<HTMLElement>(
      `[data-interaction-id="${CSS.escape(focus)}"]`,
    );
    el?.scrollIntoView({ block: "center", behavior: "auto" });
  }, [focus, focusedKey, pendingLimit]);

  const banner: PushBannerState = dismissed ? { show: false } : derivePushBanner(push);
  const enablePush = async () => {
    const result = await subscribePush();
    setPush(await readPushStatus());
    if (result.ok) {
      setShowHomeHint(false);
    } else if (result.reason === "denied") {
      dismissBanner();
      setDismissed(true);
    }
  };

  return (
    <div className={compactCss.page} data-testid="m-inbox" ref={scrollRef}>
      <header className={compactCss.top}>
        <div className={compactCss.titleRow}>
          <h1 className={compactCss.title}>收件箱</h1>
          <span className={compactCss.pending} data-testid="m-inbox-pending-count">
            待处理 {rows.pending.length}
          </span>
        </div>
        <KindSegment value={kind} onChange={onSetKind} trackClassName={compactCss.seg} />
      </header>

      {banner.show ? (
        <div className={compactCss.banner} data-testid="m-inbox-push-banner" data-mode={banner.mode}>
          <span className={compactCss.bannerText}>
            {banner.mode === "homescreen"
              ? "加到主屏幕后，收件箱才能在后台提醒你"
              : "开启通知后，收件箱才能在后台提醒你"}
          </span>
          {banner.mode === "homescreen" ? (
            <button
              type="button"
              className={compactCss.bannerAction}
              data-testid="m-inbox-push-enable"
              onClick={() => setShowHomeHint((v) => !v)}
            >
              先加到主屏幕
            </button>
          ) : (
            <button
              type="button"
              className={compactCss.bannerAction}
              data-testid="m-inbox-push-enable"
              onClick={() => void enablePush()}
            >
              开启
            </button>
          )}
          <button
            type="button"
            className={compactCss.bannerClose}
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
        <p className={compactCss.bannerHint} data-testid="m-inbox-push-hint">
          用 Safari 底部分享菜单「加到主屏幕」，再回来开启通知。
        </p>
      ) : null}

      <section className={compactCss.tier}>
        <h2 className={compactCss.tierTitle} data-testid="m-inbox-tier-pending">
          待你处理 ({rows.pending.length})
        </h2>
        {rows.pending.slice(0, pendingLimit).map((row) => {
          const item = interactionsById.get(row.interactionId);
          if (!item) return null;
          return (
            <DecisionCard key={row.interactionId} view={compactView(row, item)} mode="compact" onRespond={respond} />
          );
        })}
        {rows.pending.length === 0 ? <p className={compactCss.tierEmpty}>这里没有等你处理的交互</p> : null}
      </section>

      <section className={compactCss.tier}>
        <h2 className={compactCss.tierTitle} data-testid="m-inbox-tier-recent">
          进行中 · 最近 ({rows.recent.length})
        </h2>
        {rows.recent.slice(0, recentLimit).map((row) => (
          <RecentRowCard key={row.instanceId} row={row} />
        ))}
      </section>
    </div>
  );
}
