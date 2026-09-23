import { afterEach, expect, it, vi } from "vitest";
import { api, ScreenNodeBusyError } from "./api";
import { HubHttpError } from "./httpError";

function stubResponse(status: number, body: unknown) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
      text: async () => (typeof body === "string" ? body : JSON.stringify(body)),
    }),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it("screenRead parses a live screen body", async () => {
  stubResponse(200, { lines: ["one", "two"] });
  await expect(api.screenRead("ins_1", 80)).resolves.toEqual({ lines: ["one", "two"] });
});

it("screenRead maps expected absence (4xx) to an empty screen, not an error", async () => {
  stubResponse(422, { code: "UNSATISFIABLE", error: "host not connected" });
  await expect(api.screenRead("ins_1")).resolves.toEqual({ lines: [] });
  stubResponse(404, { code: "NOT_FOUND", error: "no screen" });
  await expect(api.screenRead("ins_1")).resolves.toEqual({ lines: [] });
});

it("screenRead surfaces NODE_BUSY/503 as the back-off error", async () => {
  stubResponse(503, { code: "NODE_BUSY", error: "busy", retryAfterMs: 1234 });
  await expect(api.screenRead("ins_1")).rejects.toBeInstanceOf(ScreenNodeBusyError);
});

it("screenRead propagates unexpected 5xx instead of silently returning an empty screen", async () => {
  stubResponse(500, { code: "INTERNAL", error: "boom" });
  await expect(api.screenRead("ins_1")).rejects.toMatchObject({
    status: 500,
    code: "INTERNAL",
  });
  expect(HubHttpError).toBeDefined();
});
