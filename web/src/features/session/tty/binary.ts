/** 32-byte TTY binary framing from protocol.md §7.4 (`tty-binary-v1`). */

export const TTY_FRAMING_VERSION = 1;
export const CHANNEL_TTY_OUTPUT = 1;
export const CHANNEL_OBJECT_CHUNK = 2;
export const HEADER_SIZE = 32;

export type TtyBinaryFrame = {
  framingVersion: number;
  channelType: number;
  streamUuid: Uint8Array;
  offset: bigint;
  payload: Uint8Array;
};

export type DecodeError = { ok: false; error: string };
export type DecodeOk = { ok: true; frame: TtyBinaryFrame };

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function uuidToBytes(uuid: string): Uint8Array | null {
  if (!UUID_RE.test(uuid)) return null;
  const hex = uuid.replace(/-/g, "");
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export function bytesToUuid(bytes: Uint8Array): string {
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** streamId is `tty_<uuid>`; header stores the 16 UUID bytes without the prefix. */
export function streamIdToUuidBytes(streamId: string): Uint8Array | null {
  const split = streamId.indexOf("_");
  if (split < 0) return null;
  return uuidToBytes(streamId.slice(split + 1));
}

export function encodeTtyOutputFrame(streamId: string, offset: bigint, payload: Uint8Array): Uint8Array {
  const uuid = streamIdToUuidBytes(streamId);
  if (!uuid) throw new Error("invalid streamId UUID");
  if (payload.byteLength > 0xffff_ffff) throw new Error("payload too large");
  const out = new Uint8Array(HEADER_SIZE + payload.byteLength);
  const view = new DataView(out.buffer);
  view.setUint8(0, TTY_FRAMING_VERSION);
  view.setUint8(1, CHANNEL_TTY_OUTPUT);
  view.setUint16(2, 0);
  out.set(uuid, 4);
  view.setBigUint64(20, offset, false);
  view.setUint32(28, payload.byteLength, false);
  out.set(payload, HEADER_SIZE);
  return out;
}

export function decodeTtyBinaryFrame(buffer: ArrayBuffer | Uint8Array): DecodeOk | DecodeError {
  const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
  if (bytes.byteLength < HEADER_SIZE) return { ok: false, error: "truncated header" };
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const framingVersion = view.getUint8(0);
  if (framingVersion !== TTY_FRAMING_VERSION) return { ok: false, error: "bad framingVersion" };
  const channelType = view.getUint8(1);
  if (channelType !== CHANNEL_TTY_OUTPUT && channelType !== CHANNEL_OBJECT_CHUNK) {
    return { ok: false, error: "bad channelType" };
  }
  if (view.getUint16(2, false) !== 0) return { ok: false, error: "reserved bytes must be 0" };
  const payloadLength = view.getUint32(28, false);
  if (bytes.byteLength !== HEADER_SIZE + payloadLength) return { ok: false, error: "payloadLength mismatch" };
  return {
    ok: true,
    frame: {
      framingVersion,
      channelType,
      streamUuid: bytes.slice(4, 20),
      offset: view.getBigUint64(20, false),
      payload: bytes.slice(HEADER_SIZE),
    },
  };
}

export function sameUuid(a: Uint8Array, b: Uint8Array): boolean {
  if (a.byteLength !== 16 || b.byteLength !== 16) return false;
  for (let i = 0; i < 16; i++) if (a[i] !== b[i]) return false;
  return true;
}
