/**
 * c-mkeybar: tty scroll position memory for the git key round trip.
 *
 * The xterm unmounts when /s/:id/tty navigates to /files and remounts on
 * return, so the position cannot live in a DOM ref (SessionPage's
 * preFilesScroll path covers the transcript scroller; the terminal is a
 * destroyed canvas). We remember the buffer LINE (baseY) rather than pixels:
 * the fresh attach replays the Hub's screen snapshot, and line index is
 * independent of pixel rounding. Only navigation through the nine-key git
 * key writes here; the value is consumed once after the terminal is ready.
 */
const memory = new Map<string, number>();

export function saveTtyScrollLine(instanceId: string, line: number): void {
  if (Number.isFinite(line) && line >= 0) memory.set(instanceId, Math.round(line));
}

export function peekTtyScrollLine(instanceId: string): number | null {
  return memory.has(instanceId) ? (memory.get(instanceId) as number) : null;
}

export function clearTtyScrollLine(instanceId: string): void {
  memory.delete(instanceId);
}
