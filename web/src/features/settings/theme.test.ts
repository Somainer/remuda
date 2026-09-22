import { afterEach, describe, expect, it, vi } from "vitest";
import { THEME_KEY, applyTheme, readTheme, writeTheme } from "./theme";

afterEach(() => {
  localStorage.removeItem(THEME_KEY);
  document.documentElement.removeAttribute("data-theme");
  vi.restoreAllMocks();
});

describe("readTheme", () => {
  it("defaults to night when nothing is stored", () => {
    expect(readTheme()).toBe("night");
  });

  it("reads a stored legal choice", () => {
    localStorage.setItem(THEME_KEY, "ledger");
    expect(readTheme()).toBe("ledger");
    localStorage.setItem(THEME_KEY, "night");
    expect(readTheme()).toBe("night");
  });

  it("falls back to night for an illegal stored value", () => {
    localStorage.setItem(THEME_KEY, "daylight");
    expect(readTheme()).toBe("night");
  });

  it("falls back to night when storage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("storage disabled");
    });
    expect(readTheme()).toBe("night");
  });
});

describe("applyTheme", () => {
  it("stamps data-theme on <html> and is idempotent", () => {
    applyTheme("ledger");
    expect(document.documentElement.dataset.theme).toBe("ledger");
    applyTheme("ledger");
    expect(document.documentElement.dataset.theme).toBe("ledger");
    applyTheme("night");
    expect(document.documentElement.dataset.theme).toBe("night");
  });
});

describe("writeTheme", () => {
  it("persists the choice and applies it", () => {
    writeTheme("ledger");
    expect(localStorage.getItem(THEME_KEY)).toBe("ledger");
    expect(document.documentElement.dataset.theme).toBe("ledger");
  });

  it("throws and leaves the DOM alone when the write is denied", () => {
    document.documentElement.dataset.theme = "night";
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => writeTheme("ledger")).toThrow("本地存储不可用");
    expect(document.documentElement.dataset.theme).toBe("night");
  });
});
