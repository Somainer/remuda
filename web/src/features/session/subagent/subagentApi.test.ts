import { afterEach, describe, expect, it, vi } from "vitest";
import { HubHttpError } from "../../../lib/httpError";
import { fetchSubagentTranscript } from "./subagentApi";

afterEach(() => {
  vi.unstubAllGlobals();
});

function respond(body: unknown, status = 200) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
      text: async () => JSON.stringify(body),
    })),
  );
}

describe("fetchSubagentTranscript", () => {
  it("treats a 200 without `available` as an unreadable host, not 「启动中」", async () => {
    // Exactly what a Node carrier with no `subagent.transcript` arm used to
    // answer (c-wfdrill2 A). Reading it as `available:false` blamed the
    // subagent for a gap in the host and left every member row 「启动中」.
    respond({ ok: true });
    await expect(fetchSubagentTranscript("ins_1", "sub123")).rejects.toBeInstanceOf(HubHttpError);
    await expect(fetchSubagentTranscript("ins_1", "sub123")).rejects.toMatchObject({
      code: "SUBAGENT_UNREADABLE",
    });
  });

  it("keeps an explicit available:false as the starting state and carries its reason", async () => {
    respond({ available: false, reason: "transcript-unbound", events: [] });
    await expect(fetchSubagentTranscript("ins_1", "sub123")).resolves.toEqual({
      available: false,
      reason: "transcript-unbound",
      events: [],
    });
  });

  it("returns the mapped transcript when the Node read one", async () => {
    respond({ available: true, meta: { agentId: "sub123", calls: 2 }, events: [] });
    const read = await fetchSubagentTranscript("ins_1", "sub123");
    expect(read.available).toBe(true);
    expect(read.meta?.agentId).toBe("sub123");
    expect(read.events).toEqual([]);
  });

  it("surfaces a transport failure as an error", async () => {
    respond({ code: "NOT_FOUND", error: "no such instance" }, 404);
    await expect(fetchSubagentTranscript("ins_1", "sub123")).rejects.toMatchObject({
      status: 404,
      code: "NOT_FOUND",
    });
  });
});
