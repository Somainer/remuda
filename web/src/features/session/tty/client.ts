import type { Instance } from "../../../types/instance";
import { readAccessCode } from "../../../lib/accessCode";
import { readSession } from "../../../lib/session";
import {
  CHANNEL_TTY_OUTPUT,
  bytesToUuid,
  concatBytes,
  decodeTtyBinaryFrame,
  encodeTtyInputFrame,
  encodeTtyOutputFrame,
  sameUuid,
  streamIdToUuidBytes,
} from "./binary";
import {
  ANSI_FIXTURE_BYTES,
  isTtyLabFixtureId,
  TTY_LAB_LEASE_ID,
  TTY_LAB_STREAM_ID,
} from "./fixture";
import { bytesFromBase64, toBytes } from "./ids";

export type TtyStatus = "connecting" | "live" | "reconnecting" | "failed" | "stale";

/**
 * A screen the browser is showing that is no longer changing.
 *
 * The Hub falls back to its own byte cache when `tty.attach` cannot be
 * answered, and those bytes are whatever it last saw — painting them as a live
 * terminal is the 2026-09-18 bug, where a frozen 17:36 frame kept a 运行中
 * header for a session whose process had been dead for an hour. `ageMs` is
 * absent when the Hub has no capture time (bytes cached before it stamped
 * them), which must read as "age unknown", never as "just now".
 */
export type TtyStale = {
  ageMs?: number;
  /** Hub reason code: `node-link-unavailable` / `instance-gone`. */
  reason?: string;
};

/** ConEmu `OSC 9;4` states the terminal header progress bar understands. */
export type TtyProgressState = "done" | "percent" | "error" | "indeterminate" | "paused";

/** Parsed `OSC 9;4` progress carried by a `tty.mode` notice. */
export type TtyProgress = {
  state: Exclude<TtyProgressState, "done">;
  percent?: number;
};

const PROGRESS_STATES: readonly TtyProgressState[] = [
  "done",
  "percent",
  "error",
  "indeterminate",
  "paused",
];

/** Validate the additive `progress` field; null means the bar must hide. */
function readProgress(value: unknown): TtyProgress | null | undefined {
  if (value === null) return null;
  if (!value || typeof value !== "object") return undefined;
  const rec = value as Record<string, unknown>;
  const state = rec.state;
  if (typeof state !== "string" || !PROGRESS_STATES.includes(state as TtyProgressState)) {
    return undefined;
  }
  if (state === "done") return null;
  const active = state as TtyProgress["state"];
  const percent =
    typeof rec.percent === "number" && Number.isFinite(rec.percent)
      ? Math.min(100, Math.max(0, rec.percent))
      : undefined;
  return percent === undefined ? { state: active } : { state: active, percent };
}

export type TtyHandlers = {
  /**
   * `replay` marks bytes the hub re-sent from the PTY ring buffer (attach
   * snapshot, reconnect backfill) rather than output the process just
   * produced. The emulator must not answer terminal queries found in those
   * bytes — the app that asked is long gone and the reply would land on
   * whatever is at the prompt now.
   */
  onFrame: (
    payload: Uint8Array,
    offset: bigint,
    streamId: string,
    reset: boolean,
    replay: boolean,
  ) => void;
  onStatus: (status: TtyStatus, message?: string, stale?: TtyStale) => void;
  onSnapshot?: () => void;
  /**
   * The attached session is (or is no longer) showing a full-screen TUI.
   *
   * Reported by the Node on attach and on mode changes (D-028 §4.6), rather
   * than sniffed from the byte stream: attaching mid-session never sees the `?1049h` that
   * put the terminal there. A fresh snapshot without mode evidence reports
   * undefined so a previous renderer observation cannot remain current.
   */
  onAltScreen?: (altScreen: boolean | undefined) => void;
  /**
   * The attached session's parsed `OSC 9;4` progress for the header bar.
   *
   * `null` is an explicit "no progress" (state 0 / attach with no evidence)
   * and hides the bar; the field simply being absent means the Node has no
   * observation and the bar stays as it was (native-config, 2026-09-16).
   */
  onProgress?: (progress: TtyProgress | null) => void;
};

export type TtySession = {
  write(data: string | Uint8Array): Promise<void>;
  resize(cols: number, rows: number): Promise<{ cols: number; rows: number }>;
  detach(): Promise<void>;
  disconnectForTest(): void;
  reconnectForTest(): void;
  /**
   * Re-ask the Hub for this session's tty snapshot.
   *
   * For a stale frame the socket is *open* — that is how the cached bytes
   * arrived — so a reconnect would be a no-op. Re-subscribing is what makes
   * the Hub try `tty.attach` again (see `send_follow_snapshot`).
   */
  retrySnapshot(): void;
};

export const TTY_INPUT_BATCH_MS = 8;
const MAX_TTY_INPUT = 4096;
const RECONNECT_MIN_MS = 400;
const RECONNECT_MAX_MS = 5000;

function mockMode(): boolean {
  return import.meta.env.VITE_MOCK === "1";
}

function hubWsUrl(path: string): string {
  // Match `api.ts` hubBase(): in Vite dev, VITE_HUB_URL is proxied same-origin
  // so the device cookie is sent. Direct-to-hub WS would be a different port.
  const raw =
    import.meta.env.DEV && import.meta.env.VITE_HUB_URL
      ? ""
      : (import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "").replace(/\/$/, "");
  if (raw.startsWith("https://")) return `${raw.replace(/^https/, "wss")}${path}`;
  if (raw.startsWith("http://")) return `${raw.replace(/^http/, "ws")}${path}`;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${path}`;
}

export function followTtyUrl(instanceId: string): string {
  const url = new URL(hubWsUrl("/v1/follow"));
  url.searchParams.set("instanceId", instanceId);
  url.searchParams.set("tty", "1");
  const token = readSession()?.token ?? readAccessCode();
  if (token) url.searchParams.set("token", token);
  return url.toString();
}

function shouldReplay(instance: Instance): boolean {
  return mockMode() || isTtyLabFixtureId(instance.id);
}

export function openTtySession(instance: Instance, handlers: TtyHandlers): TtySession {
  if (shouldReplay(instance)) return openReplaySession(handlers);
  return openLiveSession(instance, handlers);
}

function openReplaySession(handlers: TtyHandlers): TtySession {
  let streamId = TTY_LAB_STREAM_ID;
  let offset = 0n;
  let attached = true;
  let status: TtyStatus = "connecting";
  const writerLeaseId = TTY_LAB_LEASE_ID;

  const emitFixture = () => {
    if (!attached || status === "failed") return;
    const frame = encodeTtyOutputFrame(streamId, offset, ANSI_FIXTURE_BYTES);
    const decoded = decodeTtyBinaryFrame(frame);
    if (!decoded.ok) return;
    handlers.onSnapshot?.();
    handlers.onFrame(decoded.frame.payload, decoded.frame.offset, streamId, true, true);
    offset += BigInt(decoded.frame.payload.byteLength);
    status = "live";
    handlers.onStatus("live");
  };

  handlers.onStatus("connecting");
  queueMicrotask(emitFixture);

  return {
    async write(data) {
      if (!attached || status !== "live") return;
      if (!writerLeaseId || toBytes(data).byteLength === 0) return;
    },
    async resize(cols, rows) {
      return { cols, rows };
    },
    async detach() {
      attached = false;
    },
    disconnectForTest() {
      status = "reconnecting";
      handlers.onStatus("reconnecting");
    },
    reconnectForTest() {
      streamId = TTY_LAB_STREAM_ID;
      offset = 0n;
      status = "connecting";
      handlers.onStatus("connecting");
      emitFixture();
    },
    retrySnapshot() {
      // The replay fixture has no Hub behind it; re-emitting is its equivalent
      // of re-asking, and keeps the lab's reconnect affordance honest.
      emitFixture();
    },
  };
}

function asArrayBuffer(data: unknown): ArrayBuffer | null {
  if (data instanceof ArrayBuffer) return data;
  if (data instanceof Uint8Array) {
    return data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
  }
  return null;
}

function extractStreamId(value: unknown): string | null {
  if (!value || typeof value !== "object") return null;
  const rec = value as Record<string, unknown>;
  if (typeof rec.streamId === "string") return rec.streamId;
  const tty = rec.tty;
  if (tty && typeof tty === "object" && typeof (tty as { streamId?: unknown }).streamId === "string") {
    return (tty as { streamId: string }).streamId;
  }
  const params = rec.params;
  if (params && typeof params === "object" && typeof (params as { streamId?: unknown }).streamId === "string") {
    return (params as { streamId: string }).streamId;
  }
  return null;
}

function extractBase64(value: unknown): string | null {
  if (!value || typeof value !== "object") return null;
  const rec = value as Record<string, unknown>;
  if (typeof rec.dataBase64 === "string") return rec.dataBase64;
  const tty = rec.tty;
  if (tty && typeof tty === "object" && typeof (tty as { dataBase64?: unknown }).dataBase64 === "string") {
    return (tty as { dataBase64: string }).dataBase64;
  }
  const params = rec.params;
  if (params && typeof params === "object" && typeof (params as { dataBase64?: unknown }).dataBase64 === "string") {
    return (params as { dataBase64: string }).dataBase64;
  }
  return null;
}

/**
 * The staleness of a Hub-cached `tty.snapshot`, or null when it is live.
 *
 * Keyed on `source: "hub-cache"`: that marker means "these are the bytes the
 * Hub had lying around", which is stale by definition whatever else the frame
 * carries. A live attach paints the Node's own snapshot and never sets it, so
 * the marker alone separates the two. `capturedAt` is parsed to an age against
 * the browser clock — the browser and Hub already agree on wall time for every
 * other timestamp the UI renders — and a Hub too old to stamp it yields an age
 * of undefined, which the header renders as "age unknown" rather than
 * inventing one.
 */
function staleFromSnapshot(msg: Record<string, unknown>): TtyStale | null {
  if (msg.source !== "hub-cache") return null;
  const capturedAt = typeof msg.capturedAt === "string" ? msg.capturedAt : null;
  const reason = typeof msg.reason === "string" ? msg.reason : undefined;
  let ageMs: number | undefined;
  if (capturedAt) {
    const at = Date.parse(capturedAt);
    // A clock skew between browser and Hub can put `at` in the future; a
    // negative age would render as nonsense, so clamp at zero.
    if (Number.isFinite(at)) ageMs = Math.max(0, Date.now() - at);
  }
  return ageMs === undefined ? { reason } : { ageMs, reason };
}

function openLiveSession(instance: Instance, handlers: TtyHandlers): TtySession {
  let socket: WebSocket | null = null;
  let streamUuid: Uint8Array | null = null;
  let streamId = "";
  let inputOffset = 0n;
  let closed = false;
  let reconnectTimer = 0;
  let inputTimer = 0;
  let backoff = RECONNECT_MIN_MS;
  let resetNext = true;
  // The attach snapshot is replayed history. The hub sends it as an ordinary
  // binary frame — indistinguishable on the wire — so the client tracks the
  // boundary itself: the first delivery after an attach/reset is the replay,
  // everything after it is live. Reconnects and gaps re-arm it.
  let replayNext = true;
  const inputQueue: Uint8Array[] = [];
  let lastCols = 80;
  let lastRows = 24;
  let sawSnapshot = false;

  const setStream = (id: string) => {
    const uuid = streamIdToUuidBytes(id);
    if (!uuid) return;
    streamId = id;
    streamUuid = uuid;
  };

  let flushInput = () => {};

  const deliverOutput = (payload: Uint8Array, offset: bigint, id: string) => {
    const reset = resetNext;
    const replay = replayNext;
    resetNext = false;
    replayNext = false;
    handlers.onFrame(payload, offset, id, reset, replay);
  };

  const handleBinary = (buffer: ArrayBuffer) => {
    const decoded = decodeTtyBinaryFrame(buffer);
    if (!decoded.ok) return;
    if (decoded.frame.channelType !== CHANNEL_TTY_OUTPUT) return;
    if (!streamUuid) streamUuid = decoded.frame.streamUuid;
    else if (!sameUuid(decoded.frame.streamUuid, streamUuid)) return;
    if (!streamId) streamId = `tty_${bytesToUuid(decoded.frame.streamUuid)}`;
    deliverOutput(decoded.frame.payload, decoded.frame.offset, streamId);
    handlers.onStatus("live");
    if (inputQueue.length) flushInput();
  };

  const reportMode = (event: Record<string, unknown>, attached = false) => {
    const params = event.params && typeof event.params === "object"
      ? event.params as Record<string, unknown>
      : event;
    if (typeof params.instanceId === "string" && params.instanceId !== instance.id) return;
    const mode = params.altScreen;
    const hasMode = typeof mode === "boolean" || (attached && mode === null);
    // `progress` rides the same tty.mode notice (native-config). The key being
    // present is meaningful on its own: explicit null hides, absence leaves
    // the bar untouched (older Node / progress-less carrier).
    const hasProgressKey = Object.prototype.hasOwnProperty.call(params, "progress");
    const progress = hasProgressKey ? readProgress(params.progress) : undefined;
    const hasProgress = hasProgressKey && progress !== undefined;
    if (!hasMode && !hasProgress) return;
    const sid = extractStreamId(event);
    if (attached) {
      // The direct notice accompanies a fresh Hub attach. It may replace the
      // stream after recovery; subsequent nested live events cannot do that.
      if (sid && sid !== streamId) {
        if (!streamIdToUuidBytes(sid)) return;
        if (streamId) {
          resetNext = true;
          replayNext = true;
          handlers.onSnapshot?.();
        }
        setStream(sid);
      }
    } else if (!sid || !streamId || sid !== streamId) return;
    if (hasMode) {
      handlers.onAltScreen?.(typeof mode === "boolean" ? mode : undefined);
    }
    if (hasProgress) {
      handlers.onProgress?.(progress as TtyProgress | null);
    }
  };

  const handleJson = (raw: string) => {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      return;
    }
    const type = typeof msg.type === "string" ? msg.type : "";
    if (type === "tty.mode") {
      reportMode(msg, true);
      return;
    }
    if (type === "snapshot" || type === "tty.snapshot") {
      sawSnapshot = true;
      resetNext = true;
      replayNext = true;
      handlers.onSnapshot?.();
      handlers.onAltScreen?.(undefined);
      handlers.onProgress?.(null);
      streamId = "";
      streamUuid = null;
      const sid = extractStreamId(msg);
      if (sid) setStream(sid);
      const b64 = extractBase64(msg);
      if (b64) {
        const payload = bytesFromBase64(b64);
        if (payload) deliverOutput(payload, 0n, streamId || "tty_snapshot");
      }
      // A stamped snapshot is the Hub's own cache, not a live attach: the
      // bytes are the last thing it saw before the link went away. Reporting
      // it live is what let a frozen screen keep a 运行中 header, so the
      // stamp decides the status rather than being decoration on it.
      const stale = staleFromSnapshot(msg);
      if (stale) handlers.onStatus("stale", undefined, stale);
      else handlers.onStatus("live");
      return;
    }
    if (type === "gap") {
      resetNext = true;
      replayNext = true;
      handlers.onStatus("reconnecting", "gap");
      return;
    }
    const event = msg.event && typeof msg.event === "object" ? (msg.event as Record<string, unknown>) : msg;
    const eventType =
      typeof event.type === "string"
        ? event.type
        : typeof event.method === "string"
          ? event.method
          : "";
    if (eventType === "tty.mode") {
      reportMode(event);
      return;
    }
    if (eventType === "tty.frame" || type === "tty.frame") {
      const sid = extractStreamId(event) ?? extractStreamId(msg);
      if (sid) {
        setStream(sid);
        if (inputQueue.length) flushInput();
      }
      const b64 = extractBase64(event) ?? extractBase64(msg);
      if (!b64) return;
      const payload = bytesFromBase64(b64);
      if (!payload) return;
      deliverOutput(payload, 0n, streamId || sid || "tty_json");
      handlers.onStatus("live");
    }
  };

  flushInput = () => {
    inputTimer = 0;
    if (!socket || socket.readyState !== WebSocket.OPEN || !streamUuid || !inputQueue.length) return;
    const payload = concatBytes(inputQueue.splice(0));
    let rest = payload;
    while (rest.byteLength) {
      const chunk = rest.subarray(0, MAX_TTY_INPUT);
      rest = rest.subarray(chunk.byteLength);
      const frame = encodeTtyInputFrame(streamUuid, 0n, chunk);
      inputOffset += BigInt(chunk.byteLength);
      socket.send(frame.slice().buffer);
    }
  };

  const queueInput = (data: Uint8Array) => {
    if (!data.byteLength || closed) return;
    const copy = data.slice();
    inputQueue.push(copy);
    if (!inputTimer) inputTimer = window.setTimeout(flushInput, TTY_INPUT_BATCH_MS);
  };

  const sendResize = (cols: number, rows: number) => {
    lastCols = cols;
    lastRows = rows;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(JSON.stringify({ type: "tty.resize", cols, rows }));
  };

  /**
   * Ask the Hub for the tty snapshot again.
   *
   * Re-subscribing is the real retry: the Hub answers a `subscribe` with
   * `send_follow_snapshot`, which re-attempts `tty.attach` before falling back
   * to its cache. That is what a stale frame needs — the link came back and the
   * screen is still the Hub's copy of it — whereas dropping the socket would
   * only make the browser reconnect to the same live Hub.
   */
  const resubscribe = () => {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    socket.send(
      JSON.stringify({ type: "subscribe", instanceIds: [instance.id], tty: 1 }),
    );
  };

  const openSocket = () => {
    if (closed) return;
    handlers.onStatus(sawSnapshot ? "reconnecting" : "connecting");
    resetNext = true;
    replayNext = true;
    const ws = new WebSocket(followTtyUrl(instance.id));
    ws.binaryType = "arraybuffer";
    socket = ws;
    ws.addEventListener("open", () => {
      backoff = RECONNECT_MIN_MS;
      ws.send(
        JSON.stringify({
          type: "subscribe",
          instanceIds: [instance.id],
          tty: 1,
        }),
      );
      sendResize(lastCols, lastRows);
    });
    ws.addEventListener("message", (ev) => {
      if (closed || socket !== ws) return;
      if (typeof ev.data === "string") {
        handleJson(ev.data);
        return;
      }
      const buf = asArrayBuffer(ev.data);
      if (buf) handleBinary(buf);
    });
    ws.addEventListener("error", () => {
      if (closed) return;
      handlers.onStatus("reconnecting");
    });
    ws.addEventListener("close", () => {
      if (closed) return;
      handlers.onStatus("reconnecting");
      window.clearTimeout(reconnectTimer);
      reconnectTimer = window.setTimeout(openSocket, backoff);
      backoff = Math.min(RECONNECT_MAX_MS, backoff * 2);
    });
  };

  openSocket();

  return {
    async write(data) {
      queueInput(toBytes(data).slice());
    },
    async resize(cols, rows) {
      sendResize(cols, rows);
      return { cols, rows };
    },
    async detach() {
      closed = true;
      window.clearTimeout(reconnectTimer);
      window.clearTimeout(inputTimer);
      flushInput();
      socket?.close();
      socket = null;
    },
    disconnectForTest() {
      socket?.close();
    },
    reconnectForTest() {
      if (socket && socket.readyState === WebSocket.OPEN) return;
      window.clearTimeout(reconnectTimer);
      openSocket();
    },
    retrySnapshot() {
      resubscribe();
    },
  };
}

export function utf8Preview(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}
