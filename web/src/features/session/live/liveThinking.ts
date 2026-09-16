/**
 * The collapsed live "thinking" row.
 *
 * Hooks have no thinking channel on claude (design §0.3): between
 * prompt-accepted and the first tool/text anchor there is simply no
 * transcript node, so a reader scrolled off the dock cannot tell the model is
 * reasoning. The screen status line says so explicitly (`thinking with xhigh
 * effort`). This module mounts one screen-derived `thought` node — the same
 * collapsed `<details>` the transcript already renders for real thoughts —
 * while that signal is live, and removes it the instant a real thought node,
 * a tool, or streamed text exists, so it can never duplicate or outlive
 * authoritative content.
 *
 * One line in `assemble.ts` mounts this; all logic lives here.
 */
import type { Observation } from "../../../types/generated";
import type { TranscriptNode } from "../assemble";
import { livePhase } from "./phase";
import { liveStatus, phraseIsThinking } from "./liveStatus";

/** Stable id, distinct from every wire-minted node id. */
export const LIVE_THINKING_ID = "live-screen-thinking";

/** Phases after which the reasoning hint is definitively over. */
const THINKING_DONE: ReadonlySet<string> = new Set([
  "tool-started",
  "tool-output",
  "tool-finished",
  "text-streaming",
  "blocked",
  "turn-ended",
  "interrupted",
]);

/** Phases under which the screen phrase is allowed to indicate thinking. */
const THINKING_ALLOWED: ReadonlySet<string> = new Set(["prompt-accepted", "thinking"]);

function isThinkingLive(events: readonly Observation[]): boolean {
  const phase = livePhase(events);
  if (phase && THINKING_DONE.has(phase.phase)) return false;
  const status = liveStatus(events);
  if (!status?.active || !phraseIsThinking(status.phrase)) return false;
  if (phase && !THINKING_ALLOWED.has(phase.phase)) return false;
  return true;
}

/**
 * Insert or remove the screen-derived thinking node in place; returns the
 * same array for a one-line mount.
 */
export function mountLiveThinking(nodes: TranscriptNode[], events: readonly Observation[]): TranscriptNode[] {
  const existingIndex = nodes.findIndex((node) => node.type === "thought" && node.id === LIVE_THINKING_ID);
  const thinking = isThinkingLive(events);
  // A real thought node (wire id, journaled content) supersedes the hint.
  const hasRealThought = nodes.some((node) => node.type === "thought" && node.id !== LIVE_THINKING_ID);
  if (!thinking || hasRealThought) {
    if (existingIndex >= 0) nodes.splice(existingIndex, 1);
    return nodes;
  }
  if (existingIndex >= 0) return nodes;
  // Rendered by Transcript's existing thought branch: a collapsed
  // <details>, labelled as a screen guess.
  nodes.push({
    type: "thought",
    id: LIVE_THINKING_ID,
    text: "",
    completeness: "screen-derived",
  });
  return nodes;
}
