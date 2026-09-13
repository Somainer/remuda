/**
 * Replay guard: stop the emulator answering *historical* terminal queries.
 *
 * The PTY snapshot the Node keeps is a raw ring buffer of past output. When we
 * attach — first paint, reconnect, session switch, gap recovery — the hub
 * replays those bytes and we write them into xterm. If that history contains
 * terminal queries (a TUI like claude asks for CPR, DA1, OSC 10/11 colours,
 * DECRQM on startup), xterm answers them *again*, and the answers go out as
 * input. The app that asked is long gone, so they land on the shell prompt as
 * junk like `2RR0;276;0c11;rgb:1212/1616/1c1c10;rgb:...;2$y`.
 *
 * Every one of xterm's ~33 automatic replies funnels through
 * `CoreService.triggerDataEvent`, which fires `onData`. So the whole class is
 * caught by dropping `onData`/`onBinary` while replayed bytes are being parsed
 * — no sequence-by-sequence stripping needed, and live queries still get
 * answered normally.
 *
 * The timing is the subtle part. `term.write()` does NOT parse synchronously:
 * WriteBuffer defers to `setTimeout(() => this._innerWrite())` and `_innerWrite`
 * yields every 12ms, so a `flag = true; write(x); flag = false` wrapper would
 * clear the flag long before the bytes are parsed. What xterm does guarantee is
 * that each chunk's write callback runs immediately after that chunk is parsed,
 * in FIFO order with the other queued chunks. So we raise a counter when a
 * replay chunk is queued and lower it in that chunk's callback — the guard is
 * up exactly while replayed bytes are in the parser.
 *
 * A counter, not a boolean: several replay chunks can be in flight together.
 */
export type ReplayGuard = {
  /** Mark a replay chunk as queued. Call immediately before `term.write`. */
  enter: () => void;
  /** Mark it parsed. Call from that chunk's `term.write` callback. */
  leave: () => void;
  /** True while any replayed chunk is still being parsed. */
  active: () => boolean;
  /** Chunks currently in flight (tests/diagnostics). */
  depth: () => number;
};

export function createReplayGuard(): ReplayGuard {
  let depth = 0;
  return {
    enter: () => {
      depth += 1;
    },
    leave: () => {
      if (depth > 0) depth -= 1;
    },
    active: () => depth > 0,
    depth: () => depth,
  };
}

/**
 * A chunk of PTY output on its way to the emulator, tagged with where it came
 * from. `flushOut` merges queued chunks into one `term.write`, so the tag has
 * to travel with the bytes: a single flag on the writer would smear replay
 * state across live bytes batched into the same frame.
 */
export type OutChunk = {
  bytes: Uint8Array;
  replay: boolean;
};

/**
 * Split queued chunks into runs of the same origin, preserving order.
 *
 * Merging everything into one `term.write` is what makes the batching fast, but
 * a batch can hold replayed snapshot bytes *and* live bytes that arrived in the
 * same frame. Writing them together would force one guard decision over both —
 * either leaking historical answers or swallowing a live app's legitimate
 * query reply. Grouping by origin keeps each run's guard exact while still
 * collapsing the common all-live case into a single write.
 */
export function groupByOrigin(chunks: OutChunk[]): OutChunk[] {
  const runs: OutChunk[] = [];
  for (const chunk of chunks) {
    if (!chunk.bytes.byteLength) continue;
    const last = runs[runs.length - 1];
    if (last && last.replay === chunk.replay) {
      const merged = new Uint8Array(last.bytes.byteLength + chunk.bytes.byteLength);
      merged.set(last.bytes, 0);
      merged.set(chunk.bytes, last.bytes.byteLength);
      last.bytes = merged;
      continue;
    }
    runs.push({ bytes: chunk.bytes, replay: chunk.replay });
  }
  return runs;
}
