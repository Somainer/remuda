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

  // Attach the rejection handler BEFORE the deadline fires, otherwise the
  // rejection lands while nobody listens and Vitest reports it as unhandled.
  // At the 10 s read deadline the internal timer aborts the body read.
  const settled = expect(call).rejects.toThrow(/REST timeout reading body after 10000 ms/);
  await vi.advanceTimersByTimeAsync(10_000);
  await settled;
  await expect(call).rejects.toBeInstanceOf(RestNetworkError);
});

it("headers received but the error body stalls past the deadline -> RestNetworkError, not HubHttpError", async () => {
  vi.useFakeTimers();
  stubStalledBody(500, "text");

  const call = rest<unknown>("/v1/some-read");
  await vi.advanceTimersByTimeAsync(1_000);
  const settled = expect(call).rejects.toThrow(/REST timeout reading error body after 10000 ms/);
  await vi.advanceTimersByTimeAsync(10_000);
  await settled;
  await expect(call).rejects.toBeInstanceOf(RestNetworkError);
});

it("gate48 item 4: a pre-aborted CANCEL signal rejects rest() without issuing a fetch", async () => {
  const fetchMock = vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => ({}),
    text: async () => "",
  }));
  vi.stubGlobal("fetch", fetchMock);
  const controller = new AbortController();
  const reason = new DOMException("retired", "AbortError");
  controller.abort(reason);

  await expect(rest("/v1/devices", { method: "POST", body: "{}" }, controller.signal)).rejects.toBeInstanceOf(
    RestNetworkError,
  );
  await expect(
    rest("/v1/devices", { method: "POST", body: "{}" }, controller.signal),
  ).rejects.toThrow("request aborted");
  // The abort reason propagates through the error cause.
  try {
    await rest("/v1/devices", { method: "POST", body: "{}" }, controller.signal);
    throw new Error("unreachable");
  } catch (err) {
    expect((err as RestNetworkError).cause).toBe(reason);
  }
  // NEVER reached the network layer.
  expect(fetchMock).not.toHaveBeenCalled();
});

it("gate48 item 4: a pre-aborted req.signal rejects rest() without issuing a fetch", async () => {
  const fetchMock = vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => ({}),
    text: async () => "",
  }));
  vi.stubGlobal("fetch", fetchMock);
  const controller = new AbortController();
  controller.abort(new DOMException("caller cancelled", "AbortError"));

  await expect(rest("/v1/some-read", { signal: controller.signal })).rejects.toThrow("request aborted");
  expect(fetchMock).not.toHaveBeenCalled();
});

it("gate48 item 4: pre-aborted signals make api.login/pairRedeem reject without a fetch", async () => {
  const { api } = await import("./api");
  const fetchMock = vi.fn(async () => ({
    ok: true,
    status: 200,
    json: async () => ({ deviceId: "d", token: "t", name: "n" }),
    text: async () => "",
  }));
  vi.stubGlobal("fetch", fetchMock);
  const aborted = new AbortController();
  aborted.abort(new DOMException("retired", "AbortError"));

  await expect(api.login("secret", "dev", aborted.signal)).rejects.toThrow("request aborted");
  await expect(api.pairRedeem("code", "dev", aborted.signal)).rejects.toThrow("request aborted");
  expect(fetchMock).not.toHaveBeenCalled();
});

it("gate49 item 4: stalled HEADERS with a LIVE caller signal still reject at the internal read deadline", async () => {
  vi.useFakeTimers();
  // Fetch itself never resolves (headers never arrive); the caller supplies a
  // signal it NEVER aborts — the unconditional internal timer must still fire.
  const liveCallerSignal = new AbortController().signal;
  const fetchMock = vi.fn((_url: string, init: RequestInit) => {
    const signal = init.signal!;
    return new Promise<never>((_resolve, reject) => {
      signal.addEventListener(
        "abort",
        () => reject(new DOMException("The operation was aborted.", "AbortError")),
        { once: true },
      );
    });
  });
  vi.stubGlobal("fetch", fetchMock);

  const call = rest<unknown>("/v1/some-read", { signal: liveCallerSignal });
  await vi.advanceTimersByTimeAsync(1_000);
  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(liveCallerSignal.aborted).toBe(false); // the caller's signal stays live
  const settled = expect(call).rejects.toThrow(/REST timeout/);
  await vi.advanceTimersByTimeAsync(10_000);
  await settled;
});

it("gate49 item 4: stalled BODY with a LIVE caller signal still reject at the internal write deadline", async () => {
  vi.useFakeTimers();
  const liveCaller = new AbortController();
  const fetchMock = stubStalledBody(200, "json");
  // stubStalledBody ignores init.signal beyond abort plumbing; ensure the live
  // caller signal was forwarded into fetch.
  const call = rest<{ ok: boolean }>("/v1/commands", {
    method: "POST",
    body: "{}",
    signal: liveCaller.signal,
  });
  await vi.advanceTimersByTimeAsync(1_000);
  expect(fetchMock).toHaveBeenCalledTimes(1);
  // rest() drives an internal controller signal (aborted by BOTH the caller
  // signal and the internal deadline); the caller signal is forwarded via an
  // abort listener, not passed verbatim.
  expect(fetchMock.mock.calls[0]?.[1]?.signal).toBeInstanceOf(AbortSignal);
  expect(liveCaller.signal.aborted).toBe(false);
  const settled = expect(call).rejects.toThrow(/REST timeout/);
  // 15 s write deadline (not the 10 s read one) — still pending just before.
  await vi.advanceTimersByTimeAsync(14_000);
  await expect(Promise.race([call.then(() => "done"), Promise.resolve("pending")])).resolves.toBe("pending");
  await vi.advanceTimersByTimeAsync(1_000);
  await settled;
});
