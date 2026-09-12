import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEVICE_KEY } from "./interactionStatus";
import { clearSession, readLoggedOut, readSession, writeSession } from "./session";

describe("device session", () => {
  beforeEach(() => {
    clearSession();
    localStorage.clear();
  });
  afterEach(() => {
    clearSession();
    localStorage.clear();
  });

  it("round-trips a session and marks logout", () => {
    writeSession({ deviceId: "dev_1", token: "tok_1", name: "desk" });
    expect(readSession()).toEqual({ deviceId: "dev_1", token: "", name: "desk" });
    expect(JSON.parse(localStorage.getItem("runtime.device-session")!)).toEqual({ deviceId: "dev_1", name: "desk" });
    expect(localStorage.getItem("runtime.access-code")).toBeNull();
    expect(localStorage.getItem(DEVICE_KEY)).toBe("dev_1");
    expect(readLoggedOut()).toBe(false);
    clearSession();
    expect(readSession()).toBeNull();
    expect(readLoggedOut()).toBe(true);
  });

  it("restores only metadata after a module reload", async () => {
    writeSession({ deviceId: "dev_1", token: "tok_1", name: "desk" });
    vi.resetModules();
    const reloaded = await import("./session");
    expect(reloaded.readSession()).toEqual({ deviceId: "dev_1", token: "", name: "desk" });
  });

  it("retains only mock API tokens in memory", () => {
    writeSession({ deviceId: "dev_mock", token: "mock-token", name: "mock" }, { mock: true });
    expect(readSession()?.token).toBe("mock-token");
    expect(localStorage.getItem("runtime.device-session")).not.toContain("mock-token");
  });

  it("removes both legacy stored token copies without loading them into memory", () => {
    localStorage.setItem("runtime.device-session", JSON.stringify({ deviceId: "dev_legacy", name: "desk", token: "legacy-token" }));
    localStorage.setItem("runtime.access-code", "legacy-token");
    expect(readSession()).toEqual({ deviceId: "dev_legacy", name: "desk", token: "" });
    expect(localStorage.getItem("runtime.device-session")).not.toContain("legacy-token");
    expect(localStorage.getItem("runtime.access-code")).toBeNull();
  });

  it("keeps the active session in memory when storage is unavailable", () => {
    const set = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("blocked storage"); });
    writeSession({ deviceId: "dev_1", token: "tok_1", name: "desk" });
    expect(readSession()).toEqual({ deviceId: "dev_1", token: "", name: "desk" });
    clearSession();
    expect(readSession()).toBeNull();
    set.mockRestore();
  });
});
