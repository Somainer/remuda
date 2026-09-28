import { afterEach, expect, it, vi } from "vitest";
import { rest, RestNetworkError } from "./api";

/**
 * Auto-reconnect §3.1.3 owed coverage: the REST deadline covers BOTH headers
 * and the BODY. A half-open forward (or a server that answers headers then
 * stalls) must reject as a retriable RestNetworkError instead of leaving the
 * fetch — and any outbox row/Web Lock behind it — pending forever. Both the
 * success (200 .json) and error (5xx .text) body paths must honor it.
 */
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function stubStalledBody(status: number, bodyMethod: "json" | "text") {
  const fetchMock = vi.fn(async (_url: string, init: RequestInit) => {
    const signal = init.signal!;
    const stalled = new Promise<never>((_resolve, reject) => {
      signal.addEventListener(
        "abort",
        () => reject(new DOMException("The operation was aborted.", "AbortError")),
        { once: true },
      );
    });
    return {
      ok: status >= 200 && status < 300,
      status,
      json: () => (bodyMethod === "json" ? stalled : Promise.reject(new Error("not json"))),
      text: () => stalled,
    } as unknown as Response;
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

it("headers received but the success body stalls past the read deadline -> RestNetworkError", async () => {
  vi.useFakeTimers();
  const fetchMock = stubStalledBody(200, "json");

  const call = rest<{ ok: boolean }>("/v1/some-read");
  // Headers already arrived (fetch resolved); the body never completes.
  await vi.advanceTimersByTimeAsync(1_000);
  expect(fetchMock).toHaveBeenCalledTimes(1);

  // At the 10 s read deadline the internal timer aborts the body read.
  await vi.advanceTimersByTimeAsync(10_000);
  await expect(call).rejects.toBeInstanceOf(RestNetworkError);
  await expect(call).rejects.toThrow(/REST timeout reading body after 10000 ms/);
});

it("headers received but the error body stalls past the deadline -> RestNetworkError, not HubHttpError", async () => {
  vi.useFakeTimers();
  stubStalledBody(500, "text");

  const call = rest<unknown>("/v1/some-read");
  await vi.advanceTimersByTimeAsync(1_000);
  await vi.advanceTimersByTimeAsync(10_000);
  await expect(call).rejects.toBeInstanceOf(RestNetworkError);
  await expect(call).rejects.toThrow(/REST timeout reading error body after 10000 ms/);
});
