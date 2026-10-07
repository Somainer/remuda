/**
 * Reactive owner-alert watcher for questions (c-question-alert).
 *
 * One process-wide singleton, started once by the {@link QuestionAlertHost}
 * component (mounted in the shared authenticated root so a fresh `/m` or
 * `/m/inbox` entry starts it). It subscribes to the hub store and the router
 * location, derives the pending questions the owner is not viewing via
 * {@link selectQuestionAlerts}, and for each NEW one:
 *
 *  - posts a standing `notify` ("Agent <title> 有问题需要你回答") with a jump
 *    action and the live deadline, and
 *  - shows a `(n)` document.title badge until the questions are answered or
 *    expire.
 *
 * Arrival, not presence, drives the toast:
 *
 *  - the baseline is taken only after a SUCCESSFUL interaction-list
 *    hydration (`state.interactionsHydrated`) — a failed first fetch emits
 *    `ready` with an empty list, which must never baseline;
 *  - EVERY observed question id is recorded on EVERY pass, independently of
 *    whether it is alertable right now (viewed / answering), so leaving its
 *    session or finishing the answer never toasts it as "new";
 *  - a question toasts only when it was absent from the PREVIOUS PENDING set
 *    AND is alertable on this pass;
 *  - the standing notification is DISMISSED when its interaction leaves the
 *    active set (answered elsewhere, expired, …), so alerts never linger.
 *
 * The title badge and the notification deadline line are re-evaluated when
 * the soonest active deadline crosses (shared deadline clock), so a poll
 * stall cannot leave a stale count or countdown.
 *
 * Framework-free (subscribe + manual listeners) like {@link notifyStore}, so
 * it can be driven in tests without rendering React.
 */
import { hubStore } from "../../lib/store";
import { notify, notifyStore } from "../../lib/notify";
import {
  getInboxClockNow,
  subscribeInboxClock,
} from "../mobile/inboxClock";
import { formatQuestionCountdown, selectQuestionAlerts, type QuestionAlert } from "./questionAlerts";

type Unsubscribe = () => void;

const BASE_TITLE = "Remuda";
/** Re-render the countdown at most this often between deadline crossings. */
const COUNTDOWN_TICK_MS = 1000;

export class QuestionAlertWatcher {
  /** Every question id observed on any pass (alertable or not). */
  private seen = new Set<string>();
  /** Ids present as pending questions on the previous pass (alertable or
   * not): a toast needs the id absent here AND alertable now. */
  private previousPending = new Set<string>();
  private active: QuestionAlert[] = [];
  /** Posted standing-notification id per interaction, dismissed on leave. */
  private notifyIds = new Map<string, string>();
  /** Last countdown text rendered on each standing notification, so the 1 s
   * tick re-posts (same-key replace) only when the text changed. */
  private notifyCountdowns = new Map<string, string | null>();
  private hydrated = false;
  private stopHub: Unsubscribe | null = null;
  private stopRoute: Unsubscribe | null = null;
  private stopClock: Unsubscribe | null = null;
  private titleTimer: ReturnType<typeof setTimeout> | null = null;
  private titleRestored = false;
  /** Test seam: force the "active session" id instead of reading the route. */
  activeSessionOverride: string | null | undefined;
  /** Test seam: notifications posted since the last reset. */
  posted: QuestionAlert[] = [];

  /** Begin watching. Idempotent. */
  start(): void {
    if (this.stopHub) return;
    this.stopHub = hubStore.subscribe(() => this.evaluate());
    this.stopRoute = this.subscribeRoute(() => this.evaluate());
    // Shared deadline clock: re-evaluate active alerts when a deadline
    // crosses with no store emission (a stalled poll must not strand the
    // badge or a positive countdown).
    this.stopClock = subscribeInboxClock(() => this.evaluate());
    this.evaluate();
  }
  stop(): void {
    this.stopHub?.();
    this.stopRoute?.();
    this.stopClock?.();
    this.stopHub = null;
    this.stopRoute = null;
    this.stopClock = null;
    this.clearTitleTimer();
    this.restoreTitle();
  }

  /** Reset all state (tests). Also clears standing notifications it posted. */
  reset(): void {
    this.dismissAllNotifications();
    this.seen.clear();
    this.previousPending.clear();
    this.active = [];
    this.hydrated = false;
    this.posted = [];
    this.clearTitleTimer();
    this.restoreTitle();
  }

  /** Currently-active unanswered question alerts (for the title badge). */
  activeAlerts(): readonly QuestionAlert[] {
    return this.active;
  }

  /**
   * Subscribe to route changes without importing React Router. The host app
   * calls {@link setRoutePath} on navigation; defaulting to the current
   * `location.pathname` keeps it self-contained in the browser.
   */
  private subscribeRoute(listener: () => void): Unsubscribe {
    const win = globalThis.window;
    if (!win) return () => {};
    win.addEventListener("popstate", listener);
    // SPA pushes don't fire popstate; patch history once.
    patchHistoryForRoute(() => listener());
    return () => win.removeEventListener("popstate", listener);
  }

  private activeSessionId(): string | null {
    if (this.activeSessionOverride !== undefined) return this.activeSessionOverride;
    const path = globalThis.window?.location?.pathname ?? "/";
    const match = /^\/s\/([^/]+)/.exec(path);
    return match ? match[1] : null;
  }

  /** All pending question ids in the CURRENT store page, whatever their
   * alertability (viewed session, answering, paused host…). */
  private currentPendingIds(state: ReturnType<typeof hubStore.getSnapshot>): Set<string> {
    const ids = new Set<string>();
    for (const item of state.interactions) {
      if (item.kind !== "question" && item.kind !== "elicitation" && item.kind !== "plan-review") {
        continue;
      }
      // Expiry is time-derived; use the shared clock instant the rest of the
      // queue UI uses, so a crossed deadline drops the id even before the
      // next poll confirms it.
      if (item.state === "pending") ids.add(item.id);
    }
    return ids;
  }

  private evaluate(at?: number): void {
    const state = hubStore.getSnapshot();
    const now = this.clockNow(at);

    // Record EVERY observed question id on every pass — including questions
    // that are not alertable right now (the owner is viewing the session or
    // answering on this device). Leaving the session / finishing the answer
    // must not toast a question the owner already had in front of them.
    const pending = this.currentPendingIds(state);
    for (const id of pending) this.seen.add(id);

    // Baseline only after a SUCCESSFUL interaction-list hydration. A failed
    // first fetch emits ready with an empty list; baselining then would make
    // the next successful poll toast every question already pending.
    if (!this.hydrated) {
      if (!state.interactionsHydrated) {
        this.previousPending = pending;
        return;
      }
      this.hydrated = true;
      for (const id of pending) this.seen.add(id);
      this.previousPending = pending;
      // Load-time pending questions badge but do NOT toast (the owner may
      // already have seen them on another surface); no standing notification
      // is posted for them either.
      this.active = this.alertsFromState(state, now);
      this.syncTitle();
      return;
    }

    const alerts = this.alertsFromState(state, now);
    const alertIds = new Set(alerts.map((alert) => alert.interactionId));

    for (const alert of alerts) {
      const countdown = formatQuestionCountdown(alert.deadline, now);
      // Toast exactly on arrival into the ALERTABLE set: absent from the
      // previous pending page (a genuinely new question) — seen-but-viewed or
      // seen-but-answering ids were recorded on earlier passes and never
      // toast when they become alertable.
      const isNew =
        !this.previousPending.has(alert.interactionId) && !this.notifyIds.has(alert.interactionId);
      // Keep the deadline line ticking on an already-standing notification:
      // the same-key replace swaps its text without stacking, and stops
      // re-posting once there is nothing left to count down.
      const countdownChanged =
        this.notifyIds.has(alert.interactionId) &&
        this.notifyCountdowns.get(alert.interactionId) !== countdown;
      if (isNew) {
        this.seen.add(alert.interactionId);
        this.posted.push(alert);
      }
      if (isNew || countdownChanged) {
        this.postToast(alert, countdown);
      }
    }

    this.active = alerts;
    // Dismiss standing alerts whose interaction is no longer active
    // (answered on another device, expired, superseded, now being viewed).
    this.syncNotifications(alertIds);
    this.previousPending = pending;
    this.syncTitle();
  }

  private alertsFromState(
    state: ReturnType<typeof hubStore.getSnapshot>,
    now: number,
  ): QuestionAlert[] {
    return selectQuestionAlerts({
      interactions: state.interactions,
      instances: state.instances,
      hosts: state.hosts,
      activeSessionId: this.activeSessionId(),
      answering: new Set(Object.keys(state.answering)),
      titleOf: (instanceId) => hubStore.titleOf(instanceId),
      nowMs: now,
    });
  }

  /** Clock instant for a pass: an explicit tick instant (the watcher's own
   * deadline timer) or the shared deadline clock (wall clock by default). */
  private clockNow(at?: number): number {
    if (at !== undefined) return at;
    try {
      return getInboxClockNow();
    } catch {
      return Date.now();
    }
  }

  private postToast(alert: QuestionAlert, countdown: string | null): void {
    // A question needs an actual decision, so use the STANDING (blocking)
    // surface: it persists until answered/expired (dismissed below by id) or
    // the owner opens/dismisses it, unlike an `info` toast that self-hides.
    const id = notify({
      subject: `Agent ${alert.title}`,
      stage: "有问题需要你回答",
      reason: countdown ?? undefined,
      severity: "blocking",
      key: `question-alert:${alert.interactionId}`,
      diagnostic: { interactionId: alert.interactionId, instanceId: alert.instanceId },
      actions: [
        {
          id: "open",
          label: "查看",
          run: () => {
            globalThis.window?.location.assign(`/s/${alert.instanceId}`);
          },
        },
      ],
    });
    this.notifyIds.set(alert.interactionId, id);
    this.notifyCountdowns.set(alert.interactionId, countdown);
  }

  /**
   * Keep the standing notification stack consistent with the active set:
   * dismiss an alert's notification once its interaction leaves it.
   */
  private syncNotifications(activeIds: Set<string>): void {
    for (const [interactionId, notificationId] of this.notifyIds) {
      if (!activeIds.has(interactionId)) {
        notifyStore.dismiss(notificationId);
        this.notifyIds.delete(interactionId);
        this.notifyCountdowns.delete(interactionId);
      }
    }
  }

  private dismissAllNotifications(): void {
    for (const [, notificationId] of this.notifyIds) {
      notifyStore.dismiss(notificationId);
    }
    this.notifyIds.clear();
    this.notifyCountdowns.clear();
  }

  private syncTitle(): void {
    const doc = globalThis.document;
    if (!doc) return;
    const n = this.active.length;
    if (n > 0) {
      doc.title = `(${n}) ${BASE_TITLE}`;
      this.titleRestored = true;
    } else if (this.titleRestored) {
      this.restoreTitle();
    }
    this.armTitleTimer();
  }

  /**
   * Re-evaluate at the next active deadline even when no poll lands (r2 item
   * 4): the badge count must follow expiry. Arm once for the soonest
   * deadline; between crossings a 1 s tick keeps any deadline-based text in
   * step. The timer callback is a no-op when nothing changed.
   */
  private armTitleTimer(): void {
    this.clearTitleTimer();
    if (this.active.length === 0) return;
    let soonest = Number.POSITIVE_INFINITY;
    for (const alert of this.active) {
      const at = alert.deadline ? Date.parse(alert.deadline) : Number.NaN;
      if (Number.isFinite(at)) soonest = Math.min(soonest, at);
    }
    if (!Number.isFinite(soonest)) return;
    const delay = Math.max(2, Math.min(soonest - Date.now() + 2, COUNTDOWN_TICK_MS));
    this.titleTimer = setTimeout(() => {
      this.titleTimer = null;
      // Evaluate against the tick instant itself: even with no Hub poll and no
      // external clock sync, the crossed deadline must drop the badge here.
      this.evaluate(Date.now());
    }, delay);
  }

  private clearTitleTimer(): void {
    if (this.titleTimer !== null) {
      clearTimeout(this.titleTimer);
      this.titleTimer = null;
    }
  }

  private restoreTitle(): void {
    const doc = globalThis.document;
    if (doc && this.titleRestored) {
      doc.title = BASE_TITLE;
      this.titleRestored = false;
    }
  }
}

/**
 * Patch pushState/replaceState so the watcher sees SPA navigations. Installed
 * at most once per page.
 */
let historyPatched = false;
function patchHistoryForRoute(onChange: () => void): void {
  const win = globalThis.window;
  if (!win || historyPatched) return;
  historyPatched = true;
  const push = win.history.pushState;
  const replace = win.history.replaceState;
  win.history.pushState = function patchedPush(...args: Parameters<typeof push>) {
    const result = push.apply(this, args);
    onChange();
    return result;
  };
  win.history.replaceState = function patchedReplace(...args: Parameters<typeof replace>) {
    const result = replace.apply(this, args);
    onChange();
    return result;
  };
}

export const questionAlertWatcher = new QuestionAlertWatcher();
