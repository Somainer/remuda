import { describe, expect, it } from "vitest";
import {
  CHANNEL_OBJECT_CHUNK,
  CHANNEL_TTY_INPUT,
  CHANNEL_TTY_OUTPUT,
  decodeTtyBinaryFrame,
  encodeTtyInputFrame,
  encodeTtyOutputFrame,
  HEADER_SIZE,
  sameUuid,
  streamIdToUuidBytes,
} from "./binary";
import { TTY_LAB_STREAM_ID } from "./fixture";

describe("tty-binary-v1 framing", () => {
  it("round-trips a TTY output frame", () => {
    const payload = new TextEncoder().encode("hello");
    const raw = encodeTtyOutputFrame(TTY_LAB_STREAM_ID, 42n, payload);
    const decoded = decodeTtyBinaryFrame(raw);
    expect(decoded.ok).toBe(true);
    if (!decoded.ok) return;
    expect(decoded.frame.framingVersion).toBe(1);
    expect(decoded.frame.channelType).toBe(CHANNEL_TTY_OUTPUT);
    expect(decoded.frame.offset).toBe(42n);
    expect(Array.from(decoded.frame.payload)).toEqual(Array.from(payload));
    expect(sameUuid(decoded.frame.streamUuid, streamIdToUuidBytes(TTY_LAB_STREAM_ID)!)).toBe(true);
  });

  it("rejects truncated, reserved, version, and length errors", () => {
    const payload = new Uint8Array([1, 2, 3]);
    const raw = encodeTtyOutputFrame(TTY_LAB_STREAM_ID, 0n, payload);
    expect(decodeTtyBinaryFrame(raw.slice(0, HEADER_SIZE - 1)).ok).toBe(false);
    const badVersion = raw.slice();
    badVersion[0] = 2;
    expect(decodeTtyBinaryFrame(badVersion).ok).toBe(false);
    const reserved = raw.slice();
    reserved[2] = 1;
    expect(decodeTtyBinaryFrame(reserved).ok).toBe(false);
    expect(decodeTtyBinaryFrame(raw.slice(0, raw.byteLength - 1)).ok).toBe(false);
  });

  it("accepts object-chunk channel but does not treat it as TTY output", () => {
    const raw = encodeTtyOutputFrame(TTY_LAB_STREAM_ID, 0n, new Uint8Array([9]));
    raw[1] = CHANNEL_OBJECT_CHUNK;
    const decoded = decodeTtyBinaryFrame(raw);
    expect(decoded.ok).toBe(true);
    if (!decoded.ok) return;
    expect(decoded.frame.channelType).toBe(CHANNEL_OBJECT_CHUNK);
    expect(decoded.frame.channelType).not.toBe(CHANNEL_TTY_OUTPUT);
  });

  it("round-trips a TTY input frame on channel 3", () => {
    const uuid = streamIdToUuidBytes(TTY_LAB_STREAM_ID)!;
    const payload = new Uint8Array([0x1b, 0x5b, 0x4d, 0x20, 0x21, 0x21]);
    const raw = encodeTtyInputFrame(uuid, 7n, payload);
    const decoded = decodeTtyBinaryFrame(raw);
    expect(decoded.ok).toBe(true);
    if (!decoded.ok) return;
    expect(decoded.frame.channelType).toBe(CHANNEL_TTY_INPUT);
    expect(decoded.frame.offset).toBe(7n);
    expect(Array.from(decoded.frame.payload)).toEqual(Array.from(payload));
    expect(sameUuid(decoded.frame.streamUuid, uuid)).toBe(true);
  });

  it("does not apply a foreign stream UUID as this instance's input", () => {
    const mine = streamIdToUuidBytes(TTY_LAB_STREAM_ID)!;
    const other = streamIdToUuidBytes("tty_01993ab0-0000-7000-8000-00000000ffff")!;
    expect(sameUuid(mine, other)).toBe(false);
  });
});
