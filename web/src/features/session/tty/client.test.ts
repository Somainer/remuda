import { afterEach, describe, expect, it, vi } from "vitest";
import { CHANNEL_TTY_INPUT, CHANNEL_TTY_OUTPUT, decodeTtyBinaryFrame, encodeTtyOutputFrame } from "./binary";
import { followTtyUrl, openTtySession, TTY_INPUT_BATCH_MS } from "./client";
import { TTY_LAB_STREAM_ID, ttyLabInstance } from "./fixture";

class FakeSocket {
  static OPEN = 1;
  static CONNECTING = 0;
  static CLOSING = 2;
  static CLOSED = 3;
  static latest: FakeSocket | null = null;
  readyState = FakeSocket.CONNECTING;
  binaryType = "arraybuffer";
  url: string;
  sent: Array<string | ArrayBuffer | Uint8Array> = [];
  private listeners = new Map<string, Set<(ev: { data?: unknown }) => void>>();

  constructor(url: string) {
    this.url = url;
    FakeSocket.latest = this;
    queueMicrotask(() => {
      this.readyState = FakeSocket.OPEN;
      this.emit("open", {});
    });
  }

  addEventListener(type: string, fn: (ev: { data?: unknown }) => void) {
    const set = this.listeners.get(type) ?? new Set();
    set.add(fn);
    this.listeners.set(type, set);
  }

  send(data: string | ArrayBuffer | Uint8Array) {
    this.sent.push(data);
  }

  close() {
    this.readyState = FakeSocket.CLOSED;
    this.emit("close", {});
  }

  emit(type: string, ev: { data?: unknown }) {
    for (const fn of this.listeners.get(type) ?? []) fn(ev);
  }

  emitJson(value: unknown) {
    this.emit("message", { data: JSON.stringify(value) });
  }

  emitBinary(bytes: Uint8Array) {
    const copy = bytes.slice();
    this.emit("message", { data: copy.buffer });
  }
}

describe("follow tty client", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    FakeSocket.latest = null;
  });

  it("puts tty=1 on the follow URL", () => {
    const url = followTtyUrl("ins_01993ab0-0000-7000-8000-00000000bb01");
    expect(url).toContain("/v1/follow");
    expect(url).toContain("tty=1");
    expect(url).toContain("instanceId=ins_01993ab0-0000-7000-8000-00000000bb01");
  });

  it("replays snapshot then output frames and sends raw input plus resize JSON", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const frames: Uint8Array[] = [];
    const statuses: string[] = [];
    const instance = { ...ttyLabInstance(), id: "ins_01993ab0-0000-7000-8000-00000000bb01" as const };
    const session = openTtySession(instance, {
      onFrame: (payload) => {
        frames.push(payload);
      },
      onStatus: (status) => {
        statuses.push(status);
      },
    });
    await vi.waitFor(() => expect(FakeSocket.latest?.readyState).toBe(FakeSocket.OPEN));
    const ws = FakeSocket.latest!;
    ws.emitJson({
      type: "snapshot",
      instanceId: instance.id,
      asOfSeq: "0",
      events: [],
      tty: { streamId: TTY_LAB_STREAM_ID },
    });
    const hello = new TextEncoder().encode("hello-pty");
    ws.emitBinary(encodeTtyOutputFrame(TTY_LAB_STREAM_ID, 0n, hello));
    await vi.waitFor(() => expect(frames.at(-1) && new TextDecoder().decode(frames.at(-1))).toBe("hello-pty"));
    expect(statuses).toContain("live");

    await session.write("\u001b[M !!");
    await new Promise((resolve) => setTimeout(resolve, TTY_INPUT_BATCH_MS + 20));
    const binary = ws.sent.find((item) => typeof item !== "string") as Uint8Array | ArrayBuffer | undefined;
    expect(binary).toBeTruthy();
    const decoded = decodeTtyBinaryFrame(binary instanceof Uint8Array ? binary : new Uint8Array(binary!));
    expect(decoded.ok).toBe(true);
    if (decoded.ok) {
      expect(decoded.frame.channelType).toBe(CHANNEL_TTY_INPUT);
      expect(new TextDecoder().decode(decoded.frame.payload)).toBe("\u001b[M !!");
    }

    await session.resize(120, 32);
    expect(ws.sent.some((item) => typeof item === "string" && item.includes("tty.resize") && item.includes("120"))).toBe(
      true,
    );
    await session.detach();
  });

  it("accepts output channel frames and ignores object chunks", () => {
    const frame = encodeTtyOutputFrame(TTY_LAB_STREAM_ID, 1n, new Uint8Array([1]));
    const decoded = decodeTtyBinaryFrame(frame);
    expect(decoded.ok && decoded.frame.channelType === CHANNEL_TTY_OUTPUT).toBe(true);
  });

  it("surfaces the hub's tty.mode notice and treats it as its own message kind", async () => {
    // D-028 §4.6: the Node reports ?1049 on attach and the hub relays it
    // before the snapshot, so the client knows the mode while it paints.
    vi.stubGlobal("WebSocket", FakeSocket);
    const modes: boolean[] = [];
    const frames: Uint8Array[] = [];
    const instance = { ...ttyLabInstance(), id: "ins_01993ab0-0000-7000-8000-00000000bb02" as const };
    const session = openTtySession(instance, {
      onFrame: (payload) => {
        frames.push(payload);
      },
      onStatus: () => {},
      onAltScreen: (alt) => {
        modes.push(alt);
      },
    });
    await vi.waitFor(() => expect(FakeSocket.latest?.readyState).toBe(FakeSocket.OPEN));
    const ws = FakeSocket.latest!;

    ws.emitJson({ type: "tty.mode", instanceId: instance.id, altScreen: true });
    expect(modes).toEqual([true]);
    expect(frames).toHaveLength(0);

    ws.emitJson({ type: "tty.mode", instanceId: instance.id, altScreen: false });
    expect(modes).toEqual([true, false]);

    // A malformed or absent flag must not be read as `false`.
    ws.emitJson({ type: "tty.mode", instanceId: instance.id });
    ws.emitJson({ type: "tty.mode", instanceId: instance.id, altScreen: "yes" });
    expect(modes).toEqual([true, false]);
    await session.detach();
  });

  it("follows live mode events on the current stream after an in-session renderer switch", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const modes: boolean[] = [];
    const instance = { ...ttyLabInstance(), id: "ins_01993ab0-0000-7000-8000-00000000bb04" as const };
    const session = openTtySession(instance, {
      onFrame: () => {}, onStatus: () => {}, onAltScreen: (mode) => modes.push(mode),
    });
    await vi.waitFor(() => expect(FakeSocket.latest?.readyState).toBe(FakeSocket.OPEN));
    const ws = FakeSocket.latest!;
    ws.emitJson({ type: "snapshot", tty: { streamId: TTY_LAB_STREAM_ID } });
    const event = (altScreen: unknown, streamId = TTY_LAB_STREAM_ID, instanceId: string = instance.id) => ({
      type: "event", instanceId, seq: "0", event: { type: "tty.mode", params: { instanceId, streamId, altScreen } },
    });
    ws.emitJson(event(false));
    ws.emitJson(event(true));
    ws.emitJson(event(false));
    ws.emitJson(event(true, "tty_other"));
    ws.emitJson(event(true, TTY_LAB_STREAM_ID, "ins_other"));
    ws.emitJson(event(null));
    expect(modes).toEqual([false, true, false]);
    await session.detach();
  });

  it("does not require onAltScreen — an older caller keeps working", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const instance = { ...ttyLabInstance(), id: "ins_01993ab0-0000-7000-8000-00000000bb03" as const };
    const session = openTtySession(instance, { onFrame: () => {}, onStatus: () => {} });
    await vi.waitFor(() => expect(FakeSocket.latest?.readyState).toBe(FakeSocket.OPEN));
    expect(() =>
      FakeSocket.latest!.emitJson({ type: "tty.mode", instanceId: instance.id, altScreen: true }),
    ).not.toThrow();
    await session.detach();
  });
});
