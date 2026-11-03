import { afterEach, expect, it, vi } from "vitest";
import { api } from "./api";

function stubResponse(status: number, body: unknown) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
      text: async () => JSON.stringify(body),
    }),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it("hello reads a healthy Hub", async () => {
  stubResponse(200, { ok: true, version: "0.1.0", uptimeSecs: 3, tickAgeSecs: 1 });
  await expect(api.hello()).resolves.toMatchObject({ features: ["hub"] });
});

it("hello still connects when only the wake tick is stale (503 HUB_TICK_STALE)", async () => {
  stubResponse(503, { ok: false, code: "HUB_TICK_STALE", error: "the wake tick is stale" });
  await expect(api.hello()).resolves.toMatchObject({ features: ["hub"] });
});

it("hello fails on any other 503", async () => {
  stubResponse(503, { code: "NODE_BUSY", error: "busy" });
  await expect(api.hello()).rejects.toMatchObject({ status: 503, code: "NODE_BUSY" });
});
