import { describe, expect, it } from "vitest";
import { buildBroadcastBody, orderResults, summarize, type BroadcastForm } from "./broadcast";

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
