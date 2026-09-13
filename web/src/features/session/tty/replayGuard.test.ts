import { describe, expect, it } from "vitest";
import { createReplayGuard, groupByOrigin, type OutChunk } from "./replayGuard";

const enc = (text: string) => new TextEncoder().encode(text);
const dec = (bytes: Uint8Array) => new TextDecoder().decode(bytes);

/**
 * The sequence a TUI (claude) emits on startup, captured from the leaked junk
 * the user saw at the shell prompt after it exited:
 *   2RR0;276;0c11;rgb:1212/1616/1c1c10;rgb:e7e7/dcdc/c8c812;2$y
 * Those are the *answers*; what the snapshot replays is the questions.
 */
export const QUERY_HISTORY =
  "[6n" + // CPR      -> ESC[<row>;<col>R
  "[c" + // DA1      -> ESC[?1;2c
  "[>c" + // DA2      -> ESC[>0;276;0c
  "]10;?" + // OSC 10   -> rgb:....
  "]11;?" + // OSC 11   -> rgb:....
  "[?2026$p"; // DECRQM   -> ESC[?2026;2$y

describe("createReplayGuard", () => {
  it("is inactive before anything is queued", () => {
    const guard = createReplayGuard();
    expect(guard.active()).toBe(false);
    expect(guard.depth()).toBe(0);
  });

  it("is active while a replayed chunk is in the parser", () => {
    const guard = createReplayGuard();
    guard.enter();
    expect(guard.active()).toBe(true);
    guard.leave();
    expect(guard.active()).toBe(false);
  });

  it("stays active until every in-flight replay chunk has been parsed", () => {
    // xterm parses asynchronously and yields every 12ms, so several replay
    // chunks can be queued before the first callback runs. A boolean would be
    // cleared by the first callback and leak the rest.
    const guard = createReplayGuard();
    guard.enter();
    guard.enter();
    guard.enter();
    expect(guard.depth()).toBe(3);
    guard.leave();
    expect(guard.active()).toBe(true);
    guard.leave();
    expect(guard.active()).toBe(true);
    guard.leave();
    expect(guard.active()).toBe(false);
  });

  it("never goes negative if a callback fires twice", () => {
    const guard = createReplayGuard();
    guard.leave();
    guard.leave();
    expect(guard.depth()).toBe(0);
    guard.enter();
    expect(guard.active()).toBe(true);
  });
});

describe("groupByOrigin", () => {
  const chunk = (text: string, replay: boolean): OutChunk => ({ bytes: enc(text), replay });

  it("merges a run of live chunks into one write", () => {
    const runs = groupByOrigin([chunk("a", false), chunk("b", false), chunk("c", false)]);
    expect(runs).toHaveLength(1);
    expect(runs[0].replay).toBe(false);
    expect(dec(runs[0].bytes)).toBe("abc");
  });

  it("keeps replayed and live bytes in separate writes", () => {
    // The decisive case: a snapshot and the first live output can land in the
    // same animation frame. Merging them would force one guard decision over
    // both — either leaking historical answers or eating a live reply.
    const runs = groupByOrigin([
      chunk(QUERY_HISTORY, true),
      chunk("live prompt", false),
    ]);
    expect(runs.map((r) => r.replay)).toEqual([true, false]);
    expect(dec(runs[0].bytes)).toBe(QUERY_HISTORY);
    expect(dec(runs[1].bytes)).toBe("live prompt");
  });

  it("preserves order across alternating origins", () => {
    const runs = groupByOrigin([
      chunk("r1", true),
      chunk("r2", true),
      chunk("l1", false),
      chunk("r3", true),
    ]);
    expect(runs.map((r) => [dec(r.bytes), r.replay])).toEqual([
      ["r1r2", true],
      ["l1", false],
      ["r3", true],
    ]);
  });

  it("drops empty chunks without splitting a run", () => {
    const runs = groupByOrigin([chunk("a", false), chunk("", false), chunk("b", false)]);
    expect(runs).toHaveLength(1);
    expect(dec(runs[0].bytes)).toBe("ab");
  });

  it("returns nothing for an empty queue", () => {
    expect(groupByOrigin([])).toEqual([]);
  });
});

/**
 * End-to-end of the guard contract against a fake emulator that answers
 * queries the way xterm does: asynchronously, after the chunk is parsed, via
 * the same data event the input path listens to.
 */
describe("replay suppression contract", () => {
  type Write = { bytes: Uint8Array; done: () => void };

  function fakeTerminal(onData: (data: string) => void) {
    const pending: Write[] = [];
    return {
      write(bytes: Uint8Array, done: () => void) {
        pending.push({ bytes, done });
      },
      /** Parse everything queued, emitting a reply per query, then callbacks. */
      parse() {
        for (const write of pending.splice(0)) {
          const text = dec(write.bytes);
          if (text.includes("[6n")) onData("[24;1R");
          if (text.includes("[c")) onData("[?1;2c");
          if (text.includes("[>c")) onData("[>0;276;0c");
          if (text.includes("]10;?")) onData("]10;rgb:e7e7/dcdc/c8c8");
          if (text.includes("]11;?")) onData("]11;rgb:1212/1616/1c1c");
          if (text.includes("$p")) onData("[?2026;2$y");
          write.done();
        }
      },
    };
  }

  function harness() {
    const guard = createReplayGuard();
    const sent: string[] = [];
    const term = fakeTerminal((data) => {
      if (guard.active()) return;
      sent.push(data);
    });
    const flush = (queue: OutChunk[]) => {
      for (const run of groupByOrigin(queue)) {
        if (run.replay) guard.enter();
        term.write(run.bytes, () => {
          if (run.replay) guard.leave();
        });
      }
      term.parse();
    };
    return { guard, sent, flush };
  }

  it("sends nothing to the pty while replaying a query-heavy history", () => {
    const { sent, guard, flush } = harness();
    flush([{ bytes: enc(QUERY_HISTORY), replay: true }]);
    expect(sent).toEqual([]);
    expect(guard.active()).toBe(false);
  });

  it("still answers the same queries when they arrive live", () => {
    // A TUI starting up legitimately queries the terminal and needs the reply;
    // suppressing that would be a worse bug than the leak.
    const { sent, flush } = harness();
    flush([{ bytes: enc(QUERY_HISTORY), replay: false }]);
    expect(sent).toEqual([
      "[24;1R",
      "[?1;2c",
      "[>0;276;0c",
      "]10;rgb:e7e7/dcdc/c8c8",
      "]11;rgb:1212/1616/1c1c",
      "[?2026;2$y",
    ]);
  });

  it("suppresses the replayed queries but answers live ones in the same batch", () => {
    const { sent, flush } = harness();
    flush([
      { bytes: enc(QUERY_HISTORY), replay: true },
      { bytes: enc("[6n"), replay: false },
    ]);
    expect(sent).toEqual(["[24;1R"]);
  });
});
