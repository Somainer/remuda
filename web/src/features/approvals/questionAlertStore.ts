/**
 * Reactive owner-alert watcher for questions (c-question-alert).
 *
 * One process-wide singleton, started once by the {@link QuestionAlertHost}
 * component. It subscribes to the hub store and the router location, derives
 * the pending questions the owner is not viewing via {@link selectQuestionAlerts},
 * and for each NEW one:
 *
 *  - posts a toast (`notify`) with "Agent <title> 有问题需要你回答", a jump
 *    action and the deadline, and
 *  - shows a `(n)` document.title badge until the questions are answered or
 *    expire.
 *
 * Arrival, not presence, drives the toast: a `Set` of already-seen interaction
 * ids means a question never re-toasts on a poll or a route change. History
 * replay never reaches this watcher — the hub store seeds its interaction list
 * via REST/poll hydration, and we only alert on ids that appear AFTER the
 * initial hydration settles (the first subscription snapshot is treated as the
 * baseline), so opening the app with pending questions badges the count but
 * does not pop a toast for questions the owner may already have seen.
 *
 * Framework-free (subscribe + manual listeners) like {@link notifyStore}, so it
 * can be driven in tests without rendering React.
 */
import { hubStore } from "../../lib/store";
import { notify } from "../../lib/notify";
import { selectQuestionAlerts, type QuestionAlert } from "./questionAlerts";

type Unsubscribe = () => void;

const BASE_TITLE = "Remuda";

export class QuestionAlertWatcher {
  private seen = new Set<string>();
  private active: QuestionAlert[] = [];
  private hydrated = false;
  private stopHub: Unsubscribe | null = null;
  private stopRoute: Unsubscribe | null = null;
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
    this.evaluate();
  }

  stop(): void {
    this.stopHub?.();
    this.stopRoute?.();
    this.stopHub = null;
    this.stopRoute = null;
    this.restoreTitle();
  }

  /** Reset all state (tests). */
  reset(): void {
    this.seen.clear();
    this.active = [];
    this.hydrated = false;
    this.posted = [];
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

  private currentPath(): string {
    if (this.activeSessionOverride !== undefined) {
      return this.activeSessionOverride ? `/s/${this.activeSessionOverride}` : "/";
    }
    return globalThis.window?.location?.pathname ?? "/";
  }

  private activeSessionId(): string | null {
    if (this.activeSessionOverride !== undefined) return this.activeSessionOverride;
    const path = this.currentPath();
    const match = /^\/s\/([^/]+)/.exec(path);
    return match ? match[1] : null;
  }

  private evaluate(): void {
    const state = hubStore.getSnapshot();
    const alerts = selectQuestionAlerts({
      interactions: state.interactions,
      instances: state.instances,
      hosts: state.hosts,
      activeSessionId: this.activeSessionId(),
      answering: new Set(Object.keys(state.answering)),
      titleOf: (instanceId) => hubStore.titleOf(instanceId),
    });

    // The first evaluation is the baseline (REST hydration / already-present
    // questions): record ids without toasting, so history/replay and questions
    // present at load badge but do not pop a stale alert.
    if (!this.hydrated) {
      this.hydrated = true;
      for (const alert of alerts) this.seen.add(alert.interactionId);
      this.active = alerts;
      this.syncTitle();
      return;
    }

    for (const alert of alerts) {
      if (this.seen.has(alert.interactionId)) continue;
      this.seen.add(alert.interactionId);
      this.posted.push(alert);
      this.postToast(alert);
    }
    // Seen ids are retained for the watcher's lifetime: a resolved/expired
    // question must never re-toast on a later poll, and a genuinely re-asked
    // question carries a fresh interaction id.
    this.active = alerts;
    this.syncTitle();
  }

  private postToast(alert: QuestionAlert): void {
    // A question needs an actual decision, so use the STANDING (blocking)
    // surface: it persists until the owner opens/answers (or explicitly
    // dismisses), unlike an `info` toast which self-dismisses after a few
    // seconds — and unlike a transient toast it survives fake-clock advances
    // and a busy owner stepping away. The title badge is the secondary cue.
    notify({
      subject: `Agent ${alert.title}`,
      stage: "有问题需要你回答",
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
