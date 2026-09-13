import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { ttyLabInstance, TTY_LAB_INSTANCE_ID } from "../features/session/tty/fixture";
import { isGenericPty, isPromoted } from "../lib/status";
import { canShowTerminal } from "../features/session/tty/gate";
import { SessionPage } from "./SessionPage";

/** A plain `terminal` / `shell-pty` instance, before any agent starts in it. */
function terminalInstance(): Instance {
  const base = ttyLabInstance();
  return {
    ...base,
    kind: "terminal",
    driver: "shell-pty",
    mode: "native",
    promotedAt: null,
    nativeRef: { ...base.nativeRef, kind: "terminal" },
  };
}

/** The same instance after `claude` took the PTY foreground (D-025). */
function promotedInstance(): Instance {
  const base = terminalInstance();
  return {
    ...base,
    kind: "claude",
    mode: "promoted",
    promotedAt: "2026-09-13T10:00:00.000Z",
    nativeRef: { ...base.nativeRef, kind: "claude" },
  };
}

describe("terminal → agent promotion (D-025)", () => {
  it("treats only a promoted terminal as an agent session", () => {
    const terminal = terminalInstance();
    const promoted = promotedInstance();

    expect(isPromoted(terminal)).toBe(false);
    expect(isPromoted(promoted)).toBe(true);

    // A plain terminal renders the raw screen; a promoted one renders a transcript.
    expect(isGenericPty(terminal)).toBe(true);
    expect(isGenericPty(promoted)).toBe(false);
  });

  it("keeps the terminal tab available after promotion", () => {
    expect(canShowTerminal(terminalInstance())).toBe(true);
    expect(canShowTerminal(promotedInstance())).toBe(true);
  });

  it("does not promote on kind alone without mode", () => {
    // A natively created claude-pty session is an agent, but not a *promoted* one.
    expect(isPromoted(ttyLabInstance())).toBe(false);
    // And mode alone, still reading `terminal`, is not a promotion either.
    expect(isPromoted({ ...terminalInstance(), mode: "promoted" })).toBe(false);
  });
});

// jsdom has no matchMedia; the workbench viewport hook needs one to decide
// desktop vs mobile. Desktop keeps both tab links in the header.
beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }),
  });
});

/** Render SessionPage with `instance` as the only followed instance. */
async function renderSession(instance: Instance, view: "structured" | "tty") {
  const store = await import("../lib/store");
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "titleOf").mockReturnValue("terminal");
  vi.spyOn(store.hubStore, "hostName").mockReturnValue("local");
  vi.spyOn(store.hubStore, "workspaceOf").mockReturnValue(undefined);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [instance],
    events: { [instance.id]: [] },
    interactions: [],
    bubbles: [],
    hosts: [],
    journalStatus: {},
  } as ReturnType<typeof store.useHub>);
  return render(
    <MemoryRouter initialEntries={[`/s/${instance.id}/${view}`]}>
      <Routes>
        <Route path="/s/:instanceId/:view" element={<SessionPage view={view} />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("SessionPage promotion rendering", () => {
  it("shows the promoted kind in the header and the transcript in 结构", async () => {
    await renderSession(promotedInstance(), "structured");
    const page = screen.getByTestId("session-page");
    expect(page).toHaveAttribute("data-kind", "claude");
    expect(page).toHaveAttribute("data-mode", "promoted");
    expect(page).toHaveAttribute("data-driver", "shell-pty");
    expect(screen.getByTestId("promoted-badge")).toHaveTextContent("claude · promoted");
    expect(screen.getByTestId("session-driver")).toHaveTextContent("shell-pty · promoted");
    // Structured view is the agent transcript, not a screen dump.
    expect(screen.queryByTestId("screen-view")).toBeNull();
    // …and the composer is an agent prompt box, so no raw-key row.
    expect(screen.queryByTestId("keys-row")).toBeNull();
  });

  it("leaves an unpromoted terminal on the screen view with the key row", async () => {
    await renderSession(terminalInstance(), "structured");
    const page = screen.getByTestId("session-page");
    expect(page).toHaveAttribute("data-kind", "terminal");
    expect(page).toHaveAttribute("data-mode", "native");
    expect(screen.queryByTestId("promoted-badge")).toBeNull();
    expect(screen.getByTestId("screen-view")).toBeTruthy();
    expect(screen.getByTestId("keys-row")).toBeTruthy();
  });

  it("keeps the terminal tab working on a promoted instance", async () => {
    await renderSession(promotedInstance(), "structured");
    // Both tabs are offered; promotion adds the structured one, it does not
    // take the terminal away.
    expect(screen.getByRole("link", { name: "终端" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "结构" })).toBeTruthy();
  });
});

// Referenced so the lab fixture id stays wired into this file's imports.
void TTY_LAB_INSTANCE_ID;
