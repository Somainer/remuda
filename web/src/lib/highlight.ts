/**
 * On-demand syntax highlighting.
 *
 * Two things keep the PWA shell small:
 *
 * - highlight.js **core** is imported dynamically, so the highlighter itself
 *   never lands in the main chunk;
 * - each grammar is its own dynamic import and is registered on the first
 *   fence that needs it. A session that only shows plain logs pays for nothing.
 *
 * Unknown fences render plain. Highlighting is a presentation layer only:
 * callers render sanitised/escaped text either way, and highlight.js tokeniser
 * output is escaped HTML that CodeBlock inserts into a container it owns.
 */

import type { HLJSApi, LanguageFn } from "highlight.js";

type Loader = () => Promise<{ default: LanguageFn }>;

/** Blocks at or above this length render plain; tokenising streamed giants is wasted work. */
export const HIGHLIGHT_LIMIT = 10_000;

/** Canonical grammar name → lazy chunk loader. */
const LOADERS: Record<string, Loader> = {
  typescript: () => import("highlight.js/lib/languages/typescript"),
  javascript: () => import("highlight.js/lib/languages/javascript"),
  json: () => import("highlight.js/lib/languages/json"),
  rust: () => import("highlight.js/lib/languages/rust"),
  python: () => import("highlight.js/lib/languages/python"),
  bash: () => import("highlight.js/lib/languages/bash"),
};

/** Fence info strings we recognise but ship no grammar for → plain render. */
const ALIASES: Record<string, string> = {
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  cts: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  rs: "rust",
  py: "python",
  py3: "python",
  python3: "python",
  sh: "bash",
  shell: "bash",
  zsh: "bash",
};

/** Toolbar labels for canonical names. */
const LABELS: Record<string, string> = {
  typescript: "TypeScript",
  javascript: "JavaScript",
  json: "JSON",
  rust: "Rust",
  python: "Python",
  bash: "Bash",
};

type CoreHighlighter = Pick<HLJSApi, "registerLanguage" | "highlight">;

let corePromise: Promise<CoreHighlighter> | null = null;
/** Grammars registered (or in flight) on the shared core instance. */
const registered = new Map<string, Promise<void>>();

function loadCore(): Promise<CoreHighlighter> {
  corePromise ??= import("highlight.js/lib/core").then((mod) => mod.default as CoreHighlighter);
  return corePromise;
}

/**
 * Normalise a fence info string to a canonical grammar name, or null when no
 * grammar exists for it. Everything after the first whitespace (fence meta)
 * is ignored.
 */
export function resolveLanguage(info: string | null | undefined): string | null {
  if (!info) return null;
  const token = info.trim().split(/\s+/, 1)[0]!.toLowerCase();
  if (!token) return null;
  if (LOADERS[token]) return token;
  return ALIASES[token] ?? null;
}

/**
 * Human label for a fence info string: the canonical name for known grammars,
 * the raw (normalised) token for an unknown but named fence, "" for plain
 * fences. Unknown still gets a label in the toolbar — just no colour.
 */
export function languageLabel(info: string | null | undefined): string {
  const token = (info ?? "").trim().split(/\s+/, 1)[0]?.toLowerCase();
  if (!token) return "";
  const canonical = resolveLanguage(token);
  return (canonical && LABELS[canonical]) || token;
}

async function ensureLanguage(hljs: CoreHighlighter, name: string): Promise<void> {
  let loading = registered.get(name);
  if (!loading) {
    loading = LOADERS[name]!()
      .then((mod) => hljs.registerLanguage(name, mod.default))
      .catch((error) => {
        // Don't cache the failure: a flaky chunk fetch can succeed on retry.
        registered.delete(name);
        throw error;
      });
    registered.set(name, loading);
  }
  await loading;
}

/**
 * Highlight one fence body.
 *
 * Returns escaped token HTML, or null when the block should render plain:
 * no language, unknown language, over the size cap, or a tokeniser failure.
 * Never throws — a presentation nicety must not break a transcript.
 */
export async function highlightCode(info: string | null | undefined, code: string): Promise<string | null> {
  const language = resolveLanguage(info);
  if (!language || code.length >= HIGHLIGHT_LIMIT) return null;
  try {
    const hljs = await loadCore();
    await ensureLanguage(hljs, language);
    // ignoreIllegals: streamed or quoted code is often mid-token; illegal
    // matches must fall back to plain spans instead of throwing.
    return hljs.highlight(code, { language, ignoreIllegals: true }).value;
  } catch {
    return null;
  }
}
