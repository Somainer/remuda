/**
 * Lazy KaTeX loader and bounded renderer (c-math round 2).
 *
 * The engine, its stylesheet and fonts live in async chunks: a transcript with
 * no math never fetches a byte of them. The first mounted math node triggers
 * `loadMath`; every node subscribes to the shared outcome and re-renders when
 * it settles.
 *
 * Bounded work: oversized sources are not sent to KaTeX, and successful HTML
 * is memoised per (source, display) so a streaming transcript that re-renders
 * the same node on every append parses at most once.
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

/**
 * The chunk fetch. Invoked fresh on every (re)try: a failed dynamic import in
 * the browser is retried on the next `import()` call (the rejected promise is
 * not permanently memoised by the network layer).
 */
let importer: () => Promise<unknown> = () =>
  Promise.all([import("katex"), import("../components/mathKatex.css")]).then(([mod]) => mod);

/** Test seam: replace the chunk importer (e.g. reject once, then resolve). */
export function __setMathImporterForTest(fn: () => Promise<unknown>): void {
  importer = fn;
}

function setState(next: MathEngineState): void {
  state = next;
  for (const notify of listeners) notify();
}

/**
 * Kick off (or reuse) the one KaTeX chunk load. After a failure the state is
 * "error" AND the dedup slot is cleared, so the NEXT mount restarts the
 * import from "loading" instead of being stuck on the old failure.
 */
export function loadMath(): Promise<void> {
  if (state.status === "ready") return Promise.resolve();
  if (state.status === "error") {
    state = { status: "loading" };
    inFlight = null;
    setState({ status: "loading" });
  }
  inFlight ??= (async () => {
    try {
      const mod = (await importer()) as { default: KatexModule };
      setState({ status: "ready", katex: mod.default });
    } catch (error) {
      // Clear the dedup slot so a later loadMath() retries; publish error so
      // the current paint shows the source instead of an infinite spinner.
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

/** Test-only: reset the singleton (chunk-load failure tests). */
export function resetMathEngineForTest(): void {
  state = { status: "loading" };
  inFlight = null;
  importer = () =>
    Promise.all([import("katex"), import("../components/mathKatex.css")]).then(([mod]) => mod);
  renderCache.clear();
}

/** Hard caps — see round-2 review #1 and visual-system §10.3. */
export const MATH_MAX_SOURCE = 4000;
const MATH_MAX_SIZE_EM = 20;
const MATH_MAX_EXPAND = 1000;

export interface MathRenderResult {
  /** KaTeX HTML, or the empty string when the source is skipped/errored. */
  html: string;
  ok: boolean;
  /** Reason the source is shown instead ("too-large" | "parse-error" | error). */
  error?: string;
}

/** key → result, so repeated renders of the same formula don't re-parse. */
const renderCache = new Map<string, MathRenderResult>();
const CACHE_LIMIT = 500;

/**
 * Render TeX to KaTeX HTML, bounded and cached. throwOnError is false so a bad
 * formula comes back as a KaTeX error node; we report ok:false and the
 * component shows the raw source in the danger role. Sources over
 * MATH_MAX_SOURCE skip the engine entirely (a 16k expansion already costs
 * ~0.8 s and emits ~1.7 MB of HTML). Never throws.
 */
export function renderMath(katex: KatexModule, source: string, display: boolean): MathRenderResult {
  const key = `${display ? "d" : "i"}:${source}`;
  const cached = renderCache.get(key);
  if (cached) return cached;

  let result: MathRenderResult;
  if (source.length > MATH_MAX_SOURCE) {
    result = { html: "", ok: false, error: "too-large" };
  } else {
    try {
      const html = katex.renderToString(source, {
        displayMode: display,
        output: "htmlAndMathml",
        throwOnError: false,
        trust: false,
        strict: "ignore",
        maxSize: MATH_MAX_SIZE_EM,
        maxExpand: MATH_MAX_EXPAND,
      });
      const errorMatch = /class="katex-error"[^>]*title="([^"]*)"/.exec(html);
      result = errorMatch
        ? {
            html: "",
            ok: false,
            error: errorMatch[1].replace(/&quot;/g, '"').replace(/&#39;/g, "'"),
          }
        : { html, ok: true };
    } catch (error) {
      result = {
        html: "",
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  // Cache only successful renders and capped error results; bound the map so
  // a long transcript can't grow it without limit.
  if (renderCache.size >= CACHE_LIMIT) renderCache.clear();
  renderCache.set(key, result);
  return result;
}
