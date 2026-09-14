import { useEffect, useState, useSyncExternalStore } from "react";

/**
 * The notification contract (plan §2, frozen on merge).
 *
 * Two severities with deliberately different lifetimes:
 *
 * - `info` — a light confirmation. Appears briefly in a `role="status"`
 *   region, debounced, then goes away on its own. Losing one costs nothing.
 * - `blocking` — something is wrong or unproven and the user has to decide.
 *   It goes to a standing error area, stays until dismissed, and **a later
 *   success can never cover it** (exploration §5 P0-3: 关键错误不被下一条成功
 *   toast 覆盖). The 2.4-second `div` it replaces could hide a failed delete
 *   behind the next "已保存".
 *
 * Every notification names its object, its stage, and (when known) its reason,
 * so the text says what failed and where rather than just "操作失败".
 */

export type NotifySeverity = "info" | "blocking";

/** One offered action. `run` is optional so a pure-navigation action can be a link. */
export type NotifyAction = {
  id: string;
  label: string;
  run?: () => void | Promise<void>;
};

export type NotifyInput = {
  /** What this is about, in user words: "会话 alpha"、"主机 box". */
  subject: string;
  /** Which stage the fact belongs to: "删除"、"发送"、"恢复". */
  stage: string;
  /** Why, when known. Absent stays absent — no invented cause. */
  reason?: string;
  actions?: NotifyAction[];
  severity: NotifySeverity;
  /**
   * Sanitised diagnostic fields for the 复制诊断 action. Only what a reader
   * needs to identify the case; {@link formatDiagnostic} drops anything empty
   * and never serialises the whole object graph.
   */
  diagnostic?: NotifyDiagnostic;
  /**
   * Collapse key. A repeat with the same key replaces the earlier entry
   * instead of stacking. Defaults to `severity:subject:stage`, so a retry loop
   * updates one line rather than printing five.
   */
  key?: string;
};

/**
 * Identifying fields only. There is no free-form `details` member on purpose:
 * an escape hatch is how prompts, file contents and tokens end up on a
 * clipboard. Adding a field here is a deliberate act with a review attached.
 */
export type NotifyDiagnostic = {
  instanceId?: string | null;
  hostId?: string | null;
  commandId?: string | null;
  interactionId?: string | null;
  /** Short machine-readable code, e.g. `node-epoch-changed`. Not a message. */
  reasonCode?: string | null;
  /** HTTP status, when the fact came from a response. */
  httpStatus?: number | null;
  /** Display row key from `commandStatus.ts`, when one applies. */
  statusKey?: string | null;
  /** ISO timestamp of the observation. */
  at?: string | null;
};

export type Notification = NotifyInput & {
  id: string;
  severity: NotifySeverity;
  createdAt: number;
  /** One-line rendering: 对象 · 阶段 · 原因. */
  text: string;
};

export type NotifyState = {
  /** Transient confirmations. At most {@link INFO_LIMIT}, newest last. */
  info: Notification[];
  /** Standing problems. Cleared only by an explicit dismiss. */
  blocking: Notification[];
};

/** How long an `info` stays up. Matches the toast it replaces. */
export const INFO_TTL_MS = 2400;

/**
 * Debounce window for the live region (risk 4). Several `info`s inside this
 * window are announced once, so a burst of saves does not machine-gun a
 * screen reader.
 */
export const INFO_DEBOUNCE_MS = 400;

/** Info entries kept at once; older ones drop silently. */
export const INFO_LIMIT = 3;

/** Blocking entries kept at once; the oldest drops when full. */
export const BLOCKING_LIMIT = 5;

function line(input: NotifyInput): string {
  return [input.subject, input.stage, input.reason].filter(Boolean).join(" · ");
}

function keyOf(input: NotifyInput): string {
  return input.key ?? `${input.severity}:${input.subject}:${input.stage}`;
}

const DIAGNOSTIC_FIELDS: (keyof NotifyDiagnostic)[] = [
  "at",
  "statusKey",
  "reasonCode",
  "httpStatus",
  "instanceId",
  "hostId",
  "commandId",
  "interactionId",
];

/**
 * Render the 复制诊断 payload.
 *
 * Allow-listed by construction: it walks {@link DIAGNOSTIC_FIELDS} rather than
 * the object's own keys, so a caller that smuggles an extra property into
 * `diagnostic` cannot get it onto the clipboard. Empty and null fields are
 * omitted so the result has no misleading blanks.
 */
export function formatDiagnostic(notification: Notification): string {
  const lines = [`${notification.subject} · ${notification.stage}`];
  if (notification.reason) lines.push(`reason: ${notification.reason}`);
  const diagnostic = notification.diagnostic ?? {};
  for (const field of DIAGNOSTIC_FIELDS) {
    const value = diagnostic[field];
    if (value === undefined || value === null || value === "") continue;
    lines.push(`${field}: ${String(value)}`);
  }
  return lines.join("\n");
}

type Listener = () => void;

let seq = 0;

/**
 * Notification store. Deliberately framework-free so `notify()` can be called
 * from stores, loaders and event handlers — anywhere, not just inside a React
 * render.
 */
export class NotifyStore {
  private state: NotifyState = { info: [], blocking: [] };
  private listeners = new Set<Listener>();
  private timers = new Map<string, ReturnType<typeof setTimeout>>();

  getState = (): NotifyState => this.state;

  subscribe = (listener: Listener): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  private emit(next: NotifyState) {
    this.state = next;
    for (const listener of this.listeners) listener();
  }

  /** Post a notification. Returns its id so a caller can dismiss it later. */
  notify = (input: NotifyInput): string => {
    const key = keyOf(input);
    const notification: Notification = {
      ...input,
      id: `ntf_${++seq}`,
      key,
      createdAt: this.now(),
      text: line(input),
    };

    if (input.severity === "blocking") {
      // Replace same-key, then cap. Blocking entries never auto-expire: only
      // dismiss() removes them, which is what "not covered by later success"
      // means in practice.
      const kept = this.state.blocking.filter((n) => n.key !== key);
      const blocking = [...kept, notification].slice(-BLOCKING_LIMIT);
      this.emit({ ...this.state, blocking });
      return notification.id;
    }

    const kept = this.state.info.filter((n) => n.key !== key);
    const info = [...kept, notification].slice(-INFO_LIMIT);
    this.emit({ ...this.state, info });

    const previous = this.timers.get(key);
    if (previous) clearTimeout(previous);
    const timer = setTimeout(() => {
      this.timers.delete(key);
      this.emit({ ...this.state, info: this.state.info.filter((n) => n.id !== notification.id) });
    }, INFO_TTL_MS);
    this.timers.set(key, timer);
    return notification.id;
  };

  /** Remove one notification of either severity. */
  dismiss = (id: string): void => {
    this.emit({
      info: this.state.info.filter((n) => n.id !== id),
      blocking: this.state.blocking.filter((n) => n.id !== id),
    });
  };

  /** Clear every standing error. Only ever a user action. */
  dismissAllBlocking = (): void => {
    this.emit({ ...this.state, blocking: [] });
  };

  /** Test seam; avoids fake timers having to own Date too. */
  now(): number {
    return Date.now();
  }

  /** Test helper. Not used by app code. */
  reset = (): void => {
    for (const timer of this.timers.values()) clearTimeout(timer);
    this.timers.clear();
    this.emit({ info: [], blocking: [] });
  };
}

export const notifyStore = new NotifyStore();

/**
 * Post a notification. This is the signature A and B code against (plan §2).
 *
 * ```ts
 * notify({ subject: "会话 alpha", stage: "删除", severity: "info" });
 * notify({
 *   subject: "会话 alpha", stage: "删除",
 *   reason: "主机离线，数据待清理",
 *   severity: "blocking",
 *   diagnostic: { instanceId, statusKey: "record-deleted-purge-pending" },
 * });
 * ```
 */
export function notify(input: NotifyInput): string {
  return notifyStore.notify(input);
}

/**
 * Adapter for the existing `hubStore.toast(text)` callers.
 *
 * Those live in files this batch does not own (`SpacesPanel.tsx:49`,
 * `store.ts:583,619`), so they keep working unchanged and keep their plain
 * `info` behaviour. The cost is that a genuinely blocking case — the
 * `nodePurge !== "purged"` branch in `SpacesPanel.tsx:49` — is still posted as
 * `info` and still self-dismisses.
 *
 * TODO(batch D, owns SpacesPanel.tsx): replace that call with
 * `notify({ subject: 会话名, stage: "删除", reason: "主机离线，数据待清理",
 * severity: "blocking", diagnostic: { instanceId, statusKey:
 * "record-deleted-purge-pending" } })` and use `projectDeletion()` from
 * `commandStatus.ts` to decide. Same for `store.ts:619` (恢复会话失败) once C2
 * owns that file.
 */
export function toastAdapter(text: string, key?: string): string {
  return notify({ subject: text, stage: "", severity: "info", key });
}

/** Subscribe a component to the notification state. */
export function useNotifications(): NotifyState {
  return useSyncExternalStore(notifyStore.subscribe, notifyStore.getState, notifyStore.getState);
}

/**
 * Debounced text for the polite live region (risk 4).
 *
 * The region must carry short `notify()` confirmations and nothing else — a
 * streaming transcript wired into `aria-live` would announce on every chunk.
 * Debouncing collapses a burst into one announcement; the region itself only
 * ever receives {@link Notification.text}, which is a single line built from
 * subject/stage/reason.
 */
export function useLiveAnnouncement(info: Notification[], delay = INFO_DEBOUNCE_MS): string {
  const latest = info.length ? info[info.length - 1].text : "";
  const [announced, setAnnounced] = useState("");

  useEffect(() => {
    if (!latest) return;
    const timer = setTimeout(() => setAnnounced(latest), delay);
    return () => clearTimeout(timer);
  }, [latest, delay]);

  // Derived, not stored: with nothing to announce the region empties during
  // render rather than through a second render pass.
  return latest ? announced : "";
}
