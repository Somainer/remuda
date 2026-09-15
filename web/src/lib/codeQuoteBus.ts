/**
 * Tiny bus connecting a session transcript's code blocks to the mounted
 * composer draft (workbench-code-2).
 *
 * A code block rendered inside an assistant message calls {@link quoteCode}
 * when 评论 is pressed; the composer of the current session registered a
 * listener on mount. Keeping this seam out of React props avoids editing
 * Transcript.tsx/assemble.ts (batch C owns those): MarkdownText stays
 * context-free and the 评论 button simply disappears when no session composer
 * is mounted (docs, previews, mock pages).
 */
import type { CodeQuote } from "./codeAnchors";

type Listener = (quote: CodeQuote) => void;

let listener: Listener | null = null;
const targetWatchers = new Set<() => void>();

/** Subscribe to "a session composer is/isn't mounted now"; returns unsubscribe. */
export function subscribeQuoteTarget(fn: () => void): () => void {
  targetWatchers.add(fn);
  return () => targetWatchers.delete(fn);
}

function notifyTarget(): void {
  for (const fn of targetWatchers) fn();
}

/** Snapshot read for useSyncExternalStore; never a cached boolean. */
export function codeQuoteTargetSnapshot(): boolean {
  return listener !== null;
}

/** Whether a session composer is currently accepting quotes. */
export function hasCodeQuoteTarget(): boolean {
  return listener !== null;
}

/** Called by the composer; returns an unsubscribe. */
export function listenForCodeQuotes(fn: Listener): () => void {
  listener = fn;
  notifyTarget();
  return () => {
    if (listener === fn) {
      listener = null;
      notifyTarget();
    }
  };
}

/** Fire-and-forget a quote into the draft. No-op outside a session. */
export function quoteCode(quote: CodeQuote): void {
  listener?.(quote);
}
