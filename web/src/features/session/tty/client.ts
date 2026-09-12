import type { Instance } from "../../../types/instance";
import type { Id, U64 } from "../../../types/wire";
import {
  CHANNEL_TTY_OUTPUT,
  decodeTtyBinaryFrame,
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
import { bytesToBase64, newId, utf8Bytes } from "./ids";

export type TtyStatus = "connecting" | "live" | "reconnecting" | "failed";

export type TtyWriterLease = {
  leaseId: Id;
  expiresAt: string;
  inputNextSeq: U64;
};

export type TtyAttachResult = {
  streamId: Id;
  streamEpoch: Id;
  representation: "pty-bytes" | "rendered-ansi";
  nextOffset: U64;
  availableFrom: U64;
  screenSnapshotRef: Id | null;
  snapshotAtOffset: { state: "known"; value: U64 } | { state: "unknown"; reason: string; evidenceEventIds: Id[] };
  writerLease: TtyWriterLease | null;
};

export type TtyHandlers = {
  onFrame: (payload: Uint8Array, offset: bigint, streamId: string, representation: TtyAttachResult["representation"]) => void;
  onStatus: (status: TtyStatus, message?: string) => void;
};

export type TtySession = {
  write(data: string): Promise<void>;
  resize(cols: number, rows: number): Promise<{ cols: number; rows: number }>;
  detach(): Promise<void>;
  disconnectForTest(): void;
  reconnectForTest(): void;
};

const MAX_TTY_INPUT = 4096;

function mockMode(): boolean {
  return import.meta.env.VITE_MOCK === "1";
}

function hubWsUrl(): string {
  const raw = (import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "").replace(/\/$/, "");
  if (raw.startsWith("https://")) return `${raw.replace(/^https/, "wss")}/v1/client`;
  if (raw.startsWith("http://")) return `${raw.replace(/^http/, "ws")}/v1/client`;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/v1/client`;
}

function shouldReplay(instance: Instance): boolean {
  return mockMode() || isTtyLabFixtureId(instance.id);
}

export function openTtySession(instance: Instance, handlers: TtyHandlers): TtySession {
  if (shouldReplay(instance)) return openReplaySession(instance, handlers);
  return openLiveSession(instance, handlers);
}

function openReplaySession(_instance: Instance, handlers: TtyHandlers): TtySession {
  let streamId = TTY_LAB_STREAM_ID;
  let offset = 0n;
  let inputSeq = 1n;
  let resizeRevision = 1n;
  let attached = true;
  let status: TtyStatus = "connecting";
  const writerLeaseId = TTY_LAB_LEASE_ID;

  const emitFixture = () => {
    if (!attached || status === "failed") return;
    const frame = encodeTtyOutputFrame(streamId, offset, ANSI_FIXTURE_BYTES);
    const decoded = decodeTtyBinaryFrame(frame);
    if (!decoded.ok) return;
    handlers.onFrame(decoded.frame.payload, decoded.frame.offset, streamId, "rendered-ansi");
    offset += BigInt(decoded.frame.payload.byteLength);
    status = "live";
    handlers.onStatus("live");
  };

  handlers.onStatus("connecting");
  queueMicrotask(emitFixture);

  return {
    async write(data: string) {
      if (!attached || status !== "live") return;
      if (!writerLeaseId || utf8Bytes(data).byteLength === 0) return;
      inputSeq += 1n;
    },
    async resize(cols: number, rows: number) {
      resizeRevision += 1n;
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
  };
}

function openLiveSession(instance: Instance, handlers: TtyHandlers): TtySession {
  let socket: WebSocket | null = null;
  let rpcSeq = 0;
  const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
  let attach: TtyAttachResult | null = null;
  let streamUuid: Uint8Array | null = null;
  let inputSeq = 1n;
  let resizeRevision = 1n;
  let closed = false;
  let reconnectTimer = 0;

  const send = <T,>(method: string, params: unknown): Promise<T> => {
    if (!socket || socket.readyState !== WebSocket.OPEN) return Promise.reject(new Error("WSS_CLOSED"));
    const id = `t${++rpcSeq}`;
    return new Promise<T>((resolve, reject) => {
      pending.set(id, { resolve: (v) => resolve(v as T), reject });
      socket!.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  };

  const handleBinary = (buffer: ArrayBuffer) => {
    const decoded = decodeTtyBinaryFrame(buffer);
    if (!decoded.ok) return;
    if (decoded.frame.channelType !== CHANNEL_TTY_OUTPUT) return;
    if (!streamUuid || !sameUuid(decoded.frame.streamUuid, streamUuid)) return;
    if (!attach) return;
    handlers.onFrame(decoded.frame.payload, decoded.frame.offset, attach.streamId, attach.representation);
  };

  const attachNow = async () => {
    handlers.onStatus("connecting");
    attach = await send<TtyAttachResult>("tty.attach", {
      instanceId: instance.id,
      processGeneration: instance.processRef.processGeneration,
      mode: "write",
      previousStreamId: null,
      afterOffset: null,
    });
    streamUuid = streamIdToUuidBytes(attach.streamId);
    inputSeq = attach.writerLease ? BigInt(attach.writerLease.inputNextSeq) : 1n;
    handlers.onStatus("live");
  };

  const openSocket = () => {
    if (closed) return;
    const ws = new WebSocket(hubWsUrl());
    ws.binaryType = "arraybuffer";
    socket = ws;
    ws.addEventListener("open", () => {
      void send("runtime.hello", {
        protocol: { major: 1, minMinor: 0, maxMinor: 0 },
        observationSchemaMajors: [1],
        features: ["snapshot-follow-v1", "tty-binary-v1"],
      })
        .then(() => attachNow())
        .catch((err: Error) => {
          handlers.onStatus("failed", err.message);
        });
    });
    ws.addEventListener("message", (ev) => {
      if (typeof ev.data !== "string") {
        if (ev.data instanceof ArrayBuffer) handleBinary(ev.data);
        return;
      }
      const msg = JSON.parse(ev.data) as { id?: string; result?: unknown; error?: { message: string } };
      if (msg.id == null) return;
      const waiter = pending.get(String(msg.id));
      if (!waiter) return;
      pending.delete(String(msg.id));
      if (msg.error) waiter.reject(new Error(msg.error.message));
      else waiter.resolve(msg.result);
    });
    ws.addEventListener("close", () => {
      for (const waiter of pending.values()) waiter.reject(new Error("WSS_CLOSED"));
      pending.clear();
      attach = null;
      streamUuid = null;
      if (closed) return;
      handlers.onStatus("reconnecting");
      window.clearTimeout(reconnectTimer);
      reconnectTimer = window.setTimeout(openSocket, 800);
    });
  };

  openSocket();

  return {
    async write(data: string) {
      if (!attach?.writerLease) return;
      const bytes = utf8Bytes(data).slice(0, MAX_TTY_INPUT);
      const commandId = newId("cmd");
      const seq = inputSeq;
      inputSeq += 1n;
      await send("tty.write", {
        commandId,
        payload: {
          instanceId: instance.id,
          processGeneration: instance.processRef.processGeneration,
          streamId: attach.streamId,
          streamEpoch: attach.streamEpoch,
          writerLeaseId: attach.writerLease.leaseId,
          inputSeq: seq.toString(),
          dataBase64: bytesToBase64(bytes),
        },
      });
    },
    async resize(cols: number, rows: number) {
      if (!attach?.writerLease) return { cols, rows };
      resizeRevision += 1n;
      const result = await send<{ cols: number; rows: number }>("tty.resize", {
        instanceId: instance.id,
        streamId: attach.streamId,
        writerLeaseId: attach.writerLease.leaseId,
        resizeRevision: resizeRevision.toString(),
        cols,
        rows,
      });
      return result;
    },
    async detach() {
      closed = true;
      window.clearTimeout(reconnectTimer);
      if (attach && socket?.readyState === WebSocket.OPEN) {
        try {
          await send("tty.detach", {
            instanceId: instance.id,
            streamId: attach.streamId,
            writerLeaseId: attach.writerLease?.leaseId,
          });
        } catch {
          /* detach is best-effort; unmount still closes the socket */
        }
      }
      socket?.close();
      socket = null;
    },
    disconnectForTest() {
      socket?.close();
    },
    reconnectForTest() {
      if (socket && socket.readyState === WebSocket.OPEN) return;
      openSocket();
    },
  };
}
