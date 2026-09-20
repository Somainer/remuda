/**
 * Clipboard access. Write is always offered (used by 复制 buttons); read is
 * gated by secure context, API presence and the clipboard-read permission —
 * the phone nine-key 贴 button must render disabled WITH a reason instead of
 * failing silently (mobile-ui task 6 acceptance 4).
 */
export type ClipboardReadStatus =
  | { state: "ready" }
  | { state: "blocked"; reason: string };

export function clipboardReadStatusSync(): ClipboardReadStatus {
  if (typeof window === "undefined" || typeof navigator === "undefined") {
    return { state: "blocked", reason: "当前环境没有剪贴板接口" };
  }
  if (typeof navigator.clipboard?.readText !== "function") {
    return {
      state: "blocked",
      reason: window.isSecureContext
        ? "浏览器未提供剪贴板读取"
        : "剪贴板读取需要 HTTPS 安全上下文",
    };
  }
  // Optimistic until probeClipboardRead() answers: a gesture may still unlock
  // `prompt`, and denying at read time surfaces as a toast — never silent.
  return { state: "ready" };
}

/**
 * Ask the Permissions API whether clipboard-read was denied. A missing/query-
 * throwing implementation (Safari) is not a block: read is attempted on the
 * user gesture and failures are surfaced then.
 */
export async function probeClipboardRead(): Promise<ClipboardReadStatus> {
  const base = clipboardReadStatusSync();
  if (base.state === "blocked") return base;
  try {
    if (typeof navigator.permissions?.query !== "function") return { state: "ready" };
    const result = await navigator.permissions.query({
      name: "clipboard-read" as PermissionName,
    });
    if (result.state === "denied") {
      return { state: "blocked", reason: "浏览器拒绝了剪贴板读取权限" };
    }
    return { state: "ready" };
  } catch {
    return { state: "ready" };
  }
}

export function readClipboard(): Promise<string> {
  const read = navigator.clipboard?.readText;
  if (typeof read !== "function") return Promise.reject(new Error("clipboard read unavailable"));
  return read.call(navigator.clipboard);
}

export const clipboardIo = {
  read: readClipboard,
  write(text: string) {
    return navigator.clipboard?.writeText(text) ?? Promise.resolve();
  },
};
