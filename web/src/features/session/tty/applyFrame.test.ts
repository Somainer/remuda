import { describe, expect, it } from "vitest";
import { payloadForStreamWrite, RIS, stripAnsi } from "./applyFrame";
import { ANSI_FIXTURE_BYTES, ANSI_FIXTURE_TEXT } from "./fixture";

describe("rendered-ansi reset", () => {
  it("prefixes ESC c on the first write of a new stream", () => {
    const payload = new Uint8Array([0x41, 0x42]);
    const first = payloadForStreamWrite(payload, true);
    expect(Array.from(first.slice(0, 2))).toEqual(Array.from(RIS));
    expect(Array.from(first.slice(2))).toEqual([0x41, 0x42]);
    expect(payloadForStreamWrite(payload, false)).toBe(payload);
  });

  it("strips CSI from the recorded spike ANSI so the resume banner remains", () => {
    const text = stripAnsi(ANSI_FIXTURE_BYTES);
    expect(ANSI_FIXTURE_TEXT).toContain("477c322e-9208-49e1-b5d6-8f79df71cf7f");
    expect(text).toContain("Resume this session with");
    expect(text).toContain("claude --resume 477c322e-9208-49e1-b5d6-8f79df71cf7f");
    expect(text).not.toContain("\u001b");
  });
});
