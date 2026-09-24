import { describe, expect, it, vi } from "vitest";
import {
  buildBroadcastBody,
  classifyCommand,
  orderResults,
  provisionalState,
  resolveEntry,
  summarize,
  type BroadcastForm,
} from "./broadcast";
import type { components } from "../../lib/api.generated";

type CommandRecord = components["schemas"]["CommandRecord"];

function form(over: Partial<BroadcastForm> = {}): BroadcastForm {
  return { mode: "prompt", text: "PAUSE", key: "enter", filter: { hostId: "", kind: "" }, ...over };
}

describe("fleet broadcast body", () => {
  it("sends a prompt to every instance when no filter is set", () => {
    const built = buildBroadcastBody(form());
    if ("error" in built) throw new Error(built.error);
    expect(built.body.all).toBe(true);
    expect(built.body.operation).toBe("instance.send");
    expect(built.body.hosts).toBeUndefined();
    expect(built.body.kinds).toBeUndefined();
    const payload = built.body.payload as { input: { blocks: { text: string }[] } };
    expect(payload.input.blocks[0].text).toBe("PAUSE");
  });

  it("narrows by host and kind when the filter is set", () => {
    const built = buildBroadcastBody(form({ filter: { hostId: "hst_1", kind: "codex" } }));
    if ("error" in built) throw new Error(built.error);
    expect(built.body.hosts).toEqual(["hst_1"]);
    expect(built.body.kinds).toEqual(["codex"]);
  });

  it("refuses an empty prompt instead of broadcasting nothing", () => {
    const built = buildBroadcastBody(form({ text: "   " }));
    expect("error" in built && built.error).toBeTruthy();
  });

  it("encodes a key broadcast as tty.write with the PTY bytes", () => {
    const built = buildBroadcastBody(form({ mode: "key", key: "ctrl+c" }));
    if ("error" in built) throw new Error(built.error);
    expect(built.body.operation).toBe("tty.write");
    const payload = built.body.payload as { keys: string[]; dataBase64: string };
    expect(payload.keys).toEqual(["ctrl+c"]);
    // 0x03 is ETX; base64 of a single 0x03 byte.
    expect(atob(payload.dataBase64)).toBe("\x03");
  });

  it("a key broadcast does not need any text", () => {
    const built = buildBroadcastBody(form({ mode: "key", text: "" }));
    expect("error" in built).toBe(false);
  });
});

describe("fleet broadcast results", () => {
  const result = {
    accepted: 2,
    failed: 1,
    skipped: 3,
    results: [
      { instanceId: "ins_ok1", ok: true },
      { instanceId: "ins_bad", ok: false, error: "host offline" },
      { instanceId: "ins_ok2", ok: true },
    ],
  };

  it("summarizes accepted, failed and skipped", () => {
    expect(summarize(result)).toBe("已接受 2 · 失败 1 · 跳过 3");
  });

  it("lists failures first so a partial failure is visible", () => {
    expect(orderResults(result).map((entry) => entry.instanceId)).toEqual(["ins_bad", "ins_ok1", "ins_ok2"]);
  });

  it("tolerates a response with no results array", () => {
    expect(orderResults({})).toEqual([]);
    expect(summarize({})).toBe("已接受 0 · 失败 0 · 跳过 0");
  });
});

describe("fleet delivery state (D-053 §2)", () => {
  function command(over: Partial<CommandRecord>): CommandRecord {
    return {
      commandId: "cmd_1",
      instanceId: "ins_1",
      hostId: "hst_1",
      operation: "instance.send",
      state: "accepted",
      resolution: "clear",
      forwarded: false,
      idempotencyKey: null,
      payload: {},
      createdAt: "2026-09-24T00:00:00.000Z",
      updatedAt: "2026-09-24T00:00:00.000Z",
      ...over,
    };
  }

  it("the fan-out row alone is never confirmed: queued vs forwarded only", () => {
    expect(provisionalState({ ok: true, forwarded: false, state: "queued" })).toBe("queued");
    expect(provisionalState({ ok: true, forwarded: true, state: "accepted" })).toBe("forwarded");
    // A settled fleet row still carries no settlement.outcome — provisional.
    expect(provisionalState({ ok: true, forwarded: true, state: "settled", resolution: "clear" })).toBe(
      "forwarded",
    );
  });

  it("classifies the authoritative command record by settlement.outcome", () => {
    expect(classifyCommand(command({ state: "settled", resolution: "clear", forwarded: true, settlement: { outcome: "completed" } }))).toBe("confirmed");
    expect(classifyCommand(command({ state: "settled", resolution: "clear", forwarded: true, settlement: { outcome: "rejected", reason: "tool denied" } }))).toBe("failed");
    expect(classifyCommand(command({ state: "settled", resolution: "clear", forwarded: true, settlement: { outcome: "cancelled" } }))).toBe("cancelled");
  });

  it("settled/clear without an outcome is not a success", () => {
    expect(classifyCommand(command({ state: "settled", resolution: "clear", forwarded: true }))).toBe("forwarded");
  });

  it("keeps replays and transport failures distinct at the row level", () => {
    expect(provisionalState({ ok: true, replayed: true, state: "queued" })).toBe("replayed");
    expect(provisionalState({ ok: false, error: "host offline" })).toBe("failed");
  });

  it("resolveEntry reads the command once and confirms a completed settlement", async () => {
    const read = vi.fn(async () =>
      command({ state: "settled", resolution: "clear", forwarded: true, settlement: { outcome: "completed" } }),
    );
    const settled = await resolveEntry(
      { ok: true, instanceId: "ins_1", commandId: "cmd_1", hostId: "hst_1", kind: "claude", forwarded: true, state: "accepted" },
      read,
    );
    expect(settled).toEqual({ state: "confirmed", reason: undefined });
    expect(read).toHaveBeenCalledOnce();
    expect(read).toHaveBeenCalledWith("ins_1", "cmd_1");
  });

  it("resolveEntry surfaces the Node rejection reason as failure", async () => {
    const read = vi.fn(async () =>
      command({ state: "settled", resolution: "clear", forwarded: true, settlement: { outcome: "rejected", reason: "permission denied" } }),
    );
    const settled = await resolveEntry(
      { ok: true, instanceId: "ins_1", commandId: "cmd_1", hostId: "hst_1", kind: "claude", forwarded: true, state: "settled", resolution: "clear" },
      read,
    );
    expect(settled).toEqual({ state: "failed", reason: "permission denied" });
  });

  it("resolveEntry follows up briefly while the command is still open, then stays neutral", async () => {
    const read = vi
      .fn()
      .mockResolvedValueOnce(command({ state: "accepted", resolution: "clear", forwarded: true }))
      .mockResolvedValueOnce(command({ state: "accepted", resolution: "clear", forwarded: true }));
    const settled = await resolveEntry(
      { ok: true, instanceId: "ins_1", commandId: "cmd_1", hostId: "hst_1", kind: "claude", forwarded: true, state: "accepted" },
      read,
      300,
    );
    expect(settled).toEqual({ state: "forwarded" });
  });

  it("resolveEntry stays provisional on a read failure (never fabricates)", async () => {
    const read = vi.fn(async () => {
      throw new Error("404");
    });
    const settled = await resolveEntry(
      { ok: true, instanceId: "ins_1", commandId: "cmd_1", hostId: "hst_1", kind: "claude", forwarded: false, state: "queued" },
      read,
    );
    expect(settled).toEqual({ state: "queued" });
  });

  it("resolveEntry does not read for failed or replayed rows", async () => {
    const read = vi.fn();
    expect(await resolveEntry({ ok: false, error: "x" }, read)).toEqual({ state: "failed" });
    expect(await resolveEntry({ ok: true, replayed: true, state: "queued" }, read)).toEqual({ state: "replayed" });
    expect(read).not.toHaveBeenCalled();
  });
});
