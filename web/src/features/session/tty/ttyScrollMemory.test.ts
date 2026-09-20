import { afterEach, describe, expect, it } from "vitest";
import {
  clearTtyScrollLine,
  consumeTtyScrollLine,
  peekTtyScrollLine,
  saveTtyScrollLine,
} from "./ttyScrollMemory";

describe("ttyScrollMemory", () => {
  afterEach(() => clearTtyScrollLine("ins"));

  it("saves and peeks a finite non-negative line without consuming", () => {
    saveTtyScrollLine("ins", 12);
    expect(peekTtyScrollLine("ins")).toBe(12);
    // Peek is non-destructive: the restore effect re-reads until the buffer
    // can show the line.
    expect(peekTtyScrollLine("ins")).toBe(12);
  });

  it("rounds fractional lines", () => {
    saveTtyScrollLine("ins", 4.7);
    expect(peekTtyScrollLine("ins")).toBe(5);
  });

  it("ignores negative, NaN, Infinity and non-finite values", () => {
    saveTtyScrollLine("ins", -3);
    expect(peekTtyScrollLine("ins")).toBeNull();
    saveTtyScrollLine("ins", Number.NaN);
    expect(peekTtyScrollLine("ins")).toBeNull();
    saveTtyScrollLine("ins", Number.POSITIVE_INFINITY);
    expect(peekTtyScrollLine("ins")).toBeNull();
    saveTtyScrollLine("ins", Number.NEGATIVE_INFINITY);
    expect(peekTtyScrollLine("ins")).toBeNull();
  });

  it("keeps zero (a valid top-of-scrollback line)", () => {
    saveTtyScrollLine("ins", 0);
    expect(peekTtyScrollLine("ins")).toBe(0);
  });

  it("isolates instances", () => {
    saveTtyScrollLine("ins-a", 7);
    expect(peekTtyScrollLine("ins-b")).toBeNull();
    clearTtyScrollLine("ins-b");
    expect(peekTtyScrollLine("ins-a")).toBe(7);
  });

  it("consume returns null and keeps memory while the buffer is one screen", () => {
    saveTtyScrollLine("ins", 9);
    // 120 buffer rows, 120 terminal rows: nothing to scroll, keep the memory.
    expect(consumeTtyScrollLine("ins", 120, 120)).toBeNull();
    expect(peekTtyScrollLine("ins")).toBe(9);
  });

  it("consume returns the clamped line and deletes the memory once", () => {
    saveTtyScrollLine("ins", 999);
    // Buffer max scroll line is 200-34 = 166: clamp and one-shot consume.
    expect(consumeTtyScrollLine("ins", 200, 34)).toBe(166);
    expect(peekTtyScrollLine("ins")).toBeNull();
    expect(consumeTtyScrollLine("ins", 200, 34)).toBeNull();
  });

  it("consume returns the saved line unchanged when inside the range", () => {
    saveTtyScrollLine("ins", 10);
    expect(consumeTtyScrollLine("ins", 200, 34)).toBe(10);
    expect(peekTtyScrollLine("ins")).toBeNull();
  });

  it("consume on an unknown instance is a no-op", () => {
    expect(consumeTtyScrollLine("never-saved", 200, 34)).toBeNull();
  });
});
