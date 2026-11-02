import { afterEach, describe, expect, it, vi } from "vitest";
import { reportFollowVisibility, visibilityFrame } from "./followVisibility";

/** A minimal WebSocket stand-in that records what it was sent. */
class FakeSocket extends EventTarget {
  static OPEN = 1;
  readyState = 0;
  sent: string[] = [];
  send(frame: string) {
    this.sent.push(frame);
  }
  open() {
    this.readyState = FakeSocket.OPEN;
    this.dispatchEvent(new Event("open"));
  }
  close() {
    this.readyState = 3;
    this.dispatchEvent(new Event("close"));
  }
}

function setVisibility(state: "hidden" | "visible") {
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => state });
  document.dispatchEvent(new Event("visibilitychange"));
}

describe("follow visibility (D-057 §8.4)", () => {
  afterEach(() => {
    setVisibility("visible");
    vi.unstubAllGlobals();
  });

  it("reports the state on open and on every visibilitychange", () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const ws = new FakeSocket();
    reportFollowVisibility(ws as unknown as WebSocket);
    expect(ws.sent).toEqual([]);
    ws.open();
    expect(ws.sent.map((f) => JSON.parse(f))).toEqual([{ type: "visibility", state: "visible" }]);
    setVisibility("hidden");
    setVisibility("visible");
    expect(ws.sent.slice(1).map((f) => JSON.parse(f).state)).toEqual(["hidden", "visible"]);
  });

  it("a socket opened while the page is hidden says so at once", () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    setVisibility("hidden");
    const ws = new FakeSocket();
    ws.readyState = FakeSocket.OPEN;
    reportFollowVisibility(ws as unknown as WebSocket);
    expect(JSON.parse(ws.sent[0])).toEqual({ type: "visibility", state: "hidden" });
    expect(visibilityFrame()).toBe(ws.sent[0]);
  });

  it("a closed socket is sent nothing more", () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const ws = new FakeSocket();
    reportFollowVisibility(ws as unknown as WebSocket);
    ws.open();
    ws.close();
    setVisibility("hidden");
    expect(ws.sent).toHaveLength(1);
  });
});
