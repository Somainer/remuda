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

it("readAuthEpoch mints the anon epoch once per page lifetime even when storage rejects writes", async () => {
  clearSession();
  localStorage.clear();
  // get works (returns null), setItem throws for the epoch key.
  const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation((key: string) => {
    if (key === "runtime.outbox-auth-epoch") throw new DOMException("quota", "QuotaExceededError");
  });
  vi.resetModules();
  const first = await import("./session");
  const a = first.readAuthEpoch();
  const b = first.readAuthEpoch();
  expect(a).toBe(b);
  setItem.mockRestore();
  vi.resetModules();
});

it("logout rotates the persisted anon auth epoch (c-reconnfu gate 30)", async () => {
  clearSession();
  localStorage.clear();
  vi.resetModules();
  const first = await import("./session");
  const before = first.readAuthEpoch();
  expect(localStorage.getItem("runtime.outbox-auth-epoch")).toBe(before);
  first.clearSession();
  const after = first.readAuthEpoch();
  expect(after).toBeTruthy();
  expect(after).not.toBe(before);
  // The rotated epoch is the one now persisted; the old id is gone.
  expect(localStorage.getItem("runtime.outbox-auth-epoch")).toBe(after);
  vi.resetModules();
});

it("logout rotates the anon epoch even while storage cannot persist it (no cache resurrection)", async () => {
  clearSession();
  localStorage.clear();
  vi.resetModules();
  const mod = await import("./session");
  const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation((key: string) => {
    if (key === "runtime.outbox-auth-epoch") throw new DOMException("quota", "QuotaExceededError");
  });
  const before = mod.readAuthEpoch();
  expect(mod.readAuthEpoch()).toBe(before); // stable across calls
  mod.clearSession();
  const after = mod.readAuthEpoch();
  expect(after).not.toBe(before); // rotated from the page cache, not resurrected
  setItem.mockRestore();
  vi.resetModules();
});

it("GATE31 item 3: another tab's logout tombstone/rotation drops this page's cached anon epoch — it never republishes the deleted id", async () => {
  clearSession();
  localStorage.clear();
  vi.resetModules();
  const mod = await import("./session");
  const KEY = "runtime.outbox-auth-epoch";
  const e1 = mod.readAuthEpoch();
  expect(localStorage.getItem(KEY)).toBe(e1);

  // A SECOND document logs out: it writes the empty tombstone and the browser
  // delivers a storage event to this page.
  localStorage.setItem(KEY, "");
  window.dispatchEvent(new StorageEvent("storage", { key: KEY, newValue: "", oldValue: e1 }));
  // Read-before-use sees the tombstone: fresh epoch, old id not republished.
  const e2 = mod.readAuthEpoch();
  expect(e2).toBeTruthy();
  expect(e2).not.toBe(e1);
  expect(localStorage.getItem(KEY)).toBe(e2);

  // The other document then establishes its OWN new epoch (rotation while
  // live); the storage event syncs this page's cache, and a read returns the
  // remote id rather than re-writing the local one.
  const e3 = `epoch_remote_${Math.random().toString(36).slice(2)}`;
  localStorage.setItem(KEY, e3);
  window.dispatchEvent(new StorageEvent("storage", { key: KEY, newValue: e3, oldValue: e2 }));
  expect(mod.readAuthEpoch()).toBe(e3);
  expect(localStorage.getItem(KEY)).toBe(e3);

  // A second logout tombstone rotates again and is never republished.
  localStorage.setItem(KEY, "");
  window.dispatchEvent(new StorageEvent("storage", { key: KEY, newValue: "", oldValue: e3 }));
  const e4 = mod.readAuthEpoch();
  expect(e4).not.toBe(e3);
  expect(localStorage.getItem(KEY)).toBe(e4);
  vi.resetModules();
});
