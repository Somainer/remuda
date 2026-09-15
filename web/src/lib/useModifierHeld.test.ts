import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { detectPlatform, setPlatformForTest } from "./platform";
import { useModifierHeld } from "./useModifierHeld";

function modifierEvent(init: KeyboardEventInit = {}): Event {
  return new KeyboardEvent("keydown", { bubbles: true, ...init });
}

describe("useModifierHeld", () => {
  beforeEach(() => {
    setPlatformForTest(detectPlatform({ platform: "MacIntel" }));
  });

  afterEach(() => {
    setPlatformForTest(null);
  });

  it("is false until the platform modifier goes down and false again after keyup", () => {
    const { result } = renderHook(() => useModifierHeld());
    expect(result.current).toBe(false);

    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(true);

    act(() => window.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key: "Meta" })));
    expect(result.current).toBe(false);
  });

  it("watches Control instead of Meta on Windows/Linux", () => {
    setPlatformForTest(detectPlatform({ platform: "Win32" }));
    const { result } = renderHook(() => useModifierHeld());

    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(false);

    act(() => window.dispatchEvent(modifierEvent({ key: "Control" })));
    expect(result.current).toBe(true);

    act(() => window.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key: "Control" })));
    expect(result.current).toBe(false);
  });

  it("treats auto-repeated modifier keydowns as a no-op", () => {
    const { result } = renderHook(() => useModifierHeld());
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta", repeat: true })));
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta", repeat: true })));
    expect(result.current).toBe(true);
  });

  it("clears on window blur so a modifier released outside the page cannot stick", () => {
    const { result } = renderHook(() => useModifierHeld());
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(true);
    act(() => window.dispatchEvent(new Event("blur")));
    expect(result.current).toBe(false);
    // A fresh press after refocus works again.
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(true);
  });

  it("hides while focus is in a text field and re-reveals when it leaves, key still held", () => {
    const input = document.createElement("input");
    document.body.appendChild(input);
    const { result } = renderHook(() => useModifierHeld());

    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(true);

    act(() => input.focus());
    expect(result.current).toBe(false);

    act(() => input.blur());
    expect(result.current).toBe(true);

    act(() => window.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key: "Meta" })));
    expect(result.current).toBe(false);
  });

  it("also hides for select / contenteditable / .xterm targets", () => {
    const select = document.createElement("select");
    const editable = document.createElement("div");
    // setAttribute (not the contentEditable IDL setter): jsdom keeps the IDL
    // value but does not reflect it to the attribute the guard selector reads.
    editable.setAttribute("contenteditable", "true");
    const term = document.createElement("div");
    term.className = "xterm";
    const termInput = document.createElement("textarea");
    term.appendChild(termInput);
    document.body.append(select, editable, term);
    const { result } = renderHook(() => useModifierHeld());

    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    // jsdom moves real focus to form controls and to elements inside .xterm...
    for (const el of [select, termInput]) {
      act(() => el.focus());
      expect(result.current).toBe(false);
      act(() => el.blur());
      expect(result.current).toBe(true);
    }
    // ...but jsdom never focuses a contenteditable div itself; emulate the
    // browser, where it is document.activeElement.
    const active = Object.getOwnPropertyDescriptor(Document.prototype, "activeElement");
    Object.defineProperty(document, "activeElement", { configurable: true, get: () => editable });
    try {
      act(() => document.dispatchEvent(new FocusEvent("focus", { bubbles: false })));
      expect(result.current).toBe(false);
    } finally {
      Object.defineProperty(document, "activeElement", active!);
    }
    // Leaving the editor: the restored activeElement is <body>; a capture-phase
    // blur fires the recompute (jsdom cannot focus body natively).
    act(() => document.dispatchEvent(new FocusEvent("blur", { bubbles: false })));
    expect(result.current).toBe(true);
  });

  it("ignores keydowns that are part of an IME composition", () => {
    const { result } = renderHook(() => useModifierHeld());
    act(() => window.dispatchEvent(modifierEvent({ key: "Process", isComposing: true })));
    expect(result.current).toBe(false);
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta", isComposing: true })));
    expect(result.current).toBe(false);
  });

  it("never reports held on a touch platform", () => {
    setPlatformForTest(detectPlatform({ platform: "iPad", maxTouchPoints: 5 }));
    const { result } = renderHook(() => useModifierHeld());
    act(() => window.dispatchEvent(modifierEvent({ key: "Control" })));
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(false);
  });

  it("does nothing when disabled (mobile viewport)", () => {
    const { result } = renderHook(() => useModifierHeld(false));
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(false);
  });

  it("settles when the modifier chord loses either key", () => {
    const { result } = renderHook(() => useModifierHeld());
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    act(() => window.dispatchEvent(new KeyboardEvent("keyup", { bubbles: true, key: "Control" })));
    expect(result.current).toBe(false);
  });

  it("removes its listeners on unmount", () => {
    const { result, unmount } = renderHook(() => useModifierHeld());
    unmount();
    act(() => window.dispatchEvent(modifierEvent({ key: "Meta" })));
    expect(result.current).toBe(false);
  });
});
