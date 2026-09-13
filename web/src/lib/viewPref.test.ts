import { beforeEach, describe, expect, it } from "vitest";
import { readSessionView, writeSessionView } from "./viewPref";

describe("session view preference", () => {
  beforeEach(() => localStorage.clear());

  it("returns null until a view is remembered", () => {
    expect(readSessionView("ins_a")).toBeNull();
  });

  it("remembers the last choice per instance", () => {
    writeSessionView("ins_a", "structured");
    writeSessionView("ins_b", "tty");
    expect(readSessionView("ins_a")).toBe("structured");
    expect(readSessionView("ins_b")).toBe("tty");
    writeSessionView("ins_a", "tty");
    expect(readSessionView("ins_a")).toBe("tty");
  });

  it("ignores junk written by an older build", () => {
    localStorage.setItem("runtime.session-view.ins_c", "files");
    expect(readSessionView("ins_c")).toBeNull();
  });
});
