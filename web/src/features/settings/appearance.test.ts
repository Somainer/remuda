import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { APPEARANCE_KEY, applyAppearance, readAppearance, resolveAppearance, writeAppearance } from "./appearance";

type Listener = (event: MediaQueryListEvent) => void;

/** jsdom has no matchMedia; a controllable one for the system query. */
function fakeSystem(light: boolean) {
  const listeners = new Set<Listener>();
  const query = {
    matches: light,
    addEventListener: (_: string, fn: Listener) => listeners.add(fn),
    removeEventListener: (_: string, fn: Listener) => listeners.delete(fn),
  };
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => query),
  );
  return {
    listeners,
    flip(next: boolean) {
      query.matches = next;
      for (const fn of listeners) fn({ matches: next } as MediaQueryListEvent);
    },
  };
}

function addMetas() {
  for (const [media, content] of [
    ["(prefers-color-scheme: dark)", "rgb(35, 34, 32)"],
    ["(prefers-color-scheme: light)", "rgb(249, 248, 245)"],
  ]) {
    const meta = document.createElement("meta");
    meta.name = "theme-color";
    meta.media = media!;
    meta.content = content!;
    document.head.append(meta);
  }
}

const metaContents = () =>
  [...document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')].map((m) => m.content);

const root = document.documentElement;

beforeEach(() => {
  addMetas();
  root.style.setProperty("--bg-canvas", " canvas-now");
});

afterEach(() => {
  applyAppearance("dark"); // detach any system listener
  localStorage.removeItem(APPEARANCE_KEY);
  root.removeAttribute("data-appearance");
  root.removeAttribute("data-theme");
  root.removeAttribute("data-mode-switch");
  root.style.removeProperty("--bg-canvas");
  for (const meta of document.querySelectorAll('meta[name="theme-color"]')) meta.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("readAppearance", () => {
  it("defaults to system when nothing is stored", () => {
    expect(readAppearance()).toBe("system");
  });

  it("reads the stored choice", () => {
    for (const choice of ["system", "dark", "light"] as const) {
      localStorage.setItem(APPEARANCE_KEY, choice);
      expect(readAppearance()).toBe(choice);
    }
  });

  it("maps the legacy night/ledger values", () => {
    localStorage.setItem(APPEARANCE_KEY, "night");
    expect(readAppearance()).toBe("dark");
    localStorage.setItem(APPEARANCE_KEY, "ledger");
    expect(readAppearance()).toBe("light");
  });

  it("falls back to system for an unknown value or a throwing storage", () => {
    localStorage.setItem(APPEARANCE_KEY, "daylight");
    expect(readAppearance()).toBe("system");
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("storage disabled");
    });
    expect(readAppearance()).toBe("system");
  });
});

describe("applyAppearance", () => {
  it("stamps an explicit choice, mirrors data-theme and paints both metas", () => {
    applyAppearance("light");
    expect(root.dataset.appearance).toBe("light");
    expect(root.dataset.theme).toBe("ledger");
    expect(metaContents()).toEqual(["canvas-now", "canvas-now"]);
    applyAppearance("light");
    expect(root.dataset.appearance).toBe("light");
    applyAppearance("dark");
    expect(root.dataset.appearance).toBe("dark");
    expect(root.dataset.theme).toBe("night");
  });

  it("system removes the attribute and hands the metas back", () => {
    fakeSystem(false);
    applyAppearance("dark");
    applyAppearance("system");
    expect(root.hasAttribute("data-appearance")).toBe(false);
    expect(metaContents()).toEqual(["rgb(35, 34, 32)", "rgb(249, 248, 245)"]);
  });

  it("system keeps only the data-theme mirror in step with the OS", () => {
    const system = fakeSystem(true);
    applyAppearance("system");
    expect(resolveAppearance("system")).toBe("light");
    expect(root.dataset.theme).toBe("ledger");
    system.flip(false);
    expect(root.dataset.theme).toBe("night");
    expect(root.hasAttribute("data-appearance")).toBe(false);
    applyAppearance("light");
    expect(system.listeners.size).toBe(0);
  });

  it("falls back to dark when matchMedia is unavailable", () => {
    applyAppearance("system");
    expect(root.dataset.theme).toBe("night");
  });

  it("holds transitions off for two frames around a switch, including an OS flip", () => {
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (fn: FrameRequestCallback) => frames.push(fn));
    vi.stubGlobal("cancelAnimationFrame", () => undefined);
    const tick = () => frames.splice(0).forEach((fn) => fn(0));

    applyAppearance("light");
    expect(root.hasAttribute("data-mode-switch")).toBe(true);
    tick();
    expect(root.hasAttribute("data-mode-switch")).toBe(true);
    tick();
    expect(root.hasAttribute("data-mode-switch")).toBe(false);

    const system = fakeSystem(false);
    applyAppearance("system");
    tick();
    tick();
    system.flip(true);
    expect(root.hasAttribute("data-mode-switch")).toBe(true);
    tick();
    tick();
    expect(root.hasAttribute("data-mode-switch")).toBe(false);
  });
});

describe("writeAppearance", () => {
  it("persists the choice and applies it", () => {
    writeAppearance("light");
    expect(localStorage.getItem(APPEARANCE_KEY)).toBe("light");
    expect(root.dataset.appearance).toBe("light");
  });

  it("throws and leaves the DOM alone when the write is denied", () => {
    applyAppearance("dark");
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => writeAppearance("light")).toThrow("本地存储不可用");
    expect(root.dataset.appearance).toBe("dark");
    expect(root.dataset.theme).toBe("night");
  });
});
