/**
 * Lazy KaTeX loader (c-math). The engine, its stylesheet and its fonts live in
 * async chunks: a transcript with no math never fetches a byte of them. The
 * first mounted math node triggers `loadMath`; every node subscribes to the
 * shared outcome and re-renders when it settles.
 *
 * KaTeX output is generated HTML (never user-supplied markup) with
 * trust:false, so the MathBlock component can insert it safely.
 */

import type katexType from "katex";

export type KatexModule = typeof katexType;

export type MathEngineState =
  | { status: "loading" }
  | { status: "ready"; katex: KatexModule }
  | { status: "error"; message: string };

let state: MathEngineState = { status: "loading" };
let inFlight: Promise<void> | null = null;
const listeners = new Set<() => void>();

function setState(next: MathEngineState): void {
  state = next;
  for (const notify of listeners) notify();
}

/** Kick off (or reuse) the one KaTeX chunk load. Never rejects. */
export function loadMath(): Promise<void> {
  if (state.status !== "loading") return Promise.resolve();
  inFlight ??= (async () => {
    try {
      const [mod] = await Promise.all([import("katex"), import("../components/mathKatex.css")]);
      setState({ status: "ready", katex: mod.default });
    } catch (error) {
      // Don't cache a failure forever: allow a later render to retry.
      inFlight = null;
      setState({
        status: "error",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  })();
  return inFlight;
}

export function subscribeMath(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => listeners.delete(onChange);
}

export function getMathState(): MathEngineState {
  return state;
}

export interface MathRenderResult {
  /** KaTeX HTML, or the raw source span for a parse error. */
  html: string;
  ok: boolean;
  /** Parse-error message when ok is false. */
  error?: string;
}

/**
 * Render TeX to KaTeX HTML. throwOnError is false, so a bad formula comes
 * back marked with `katex-error`; we surface that as ok:false so the
 * component can show the raw source in the danger role instead of KaTeX's
 * hard-coded red. Never throws.
 */
export function renderMath(katex: KatexModule, source: string, display: boolean): MathRenderResult {
  try {
    const html = katex.renderToString(source, {
      displayMode: display,
      output: "htmlAndMathml",
      throwOnError: false,
      trust: false,
      strict: "ignore",
    });
    const errorMatch = /class="katex-error"[^>]*title="([^"]*)"/.exec(html);
    if (errorMatch) {
      return {
        html: "",
        ok: false,
        error: errorMatch[1].replace(/&quot;/g, '"').replace(/&#39;/g, "'"),
      };
    }
    return { html, ok: true };
  } catch (error) {
    return {
      html: "",
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}
