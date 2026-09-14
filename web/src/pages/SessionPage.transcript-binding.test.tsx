import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import type { Observation } from "../types/generated";
import { ttyLabInstance } from "../features/session/tty/fixture";
import { SessionPage } from "./SessionPage";

function promotedInstance(): Instance {
  const base = ttyLabInstance();
  return {
    ...base,
    kind: "claude",
    driver: "shell-pty",
    mode: "promoted",
    promotedAt: "2026-09-14T10:00:00.000Z",
    nativeRef: { ...base.nativeRef, kind: "claude" },
  };
}

function lifecycle(nativeName: string, relatedIds: Record<string, string> = {}): Observation {
  return {
    kind: "lifecycle",
    payload: {
      type: "native",
      nativeName,
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: nativeName },
      relatedIds,
    },
  } as unknown as Observation;
}

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

async function renderSession(instance: Instance, events: Observation[]) {
  const store = await import("../lib/store");
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "titleOf").mockReturnValue("terminal");
  vi.spyOn(store.hubStore, "hostName").mockReturnValue("local");
  vi.spyOn(store.hubStore, "workspaceOf").mockReturnValue(undefined);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [instance],
    events: { [instance.id]: events },
    interactions: [],
    bubbles: [],
    hosts: [],
    journalStatus: {},
  } as ReturnType<typeof store.useHub>);
  return render(
    <MemoryRouter initialEntries={[`/s/${instance.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/:view" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("promoted transcript binding chip (D-025)", () => {
  it("shows the bound session short id and the binding channel", async () => {
    await renderSession(
      promotedInstance(),
      [
        lifecycle("transcript_bound", {
          sessionId: "f121381b-aaaa-4bbb-8ccc-000000000000",
          source: "hook",
          transcriptPath: "/tmp/f121381b.jsonl",
        }),
      ],
    );
    const chip = screen.getByTestId("transcript-binding");
    expect(chip).toHaveAttribute("data-state", "bound");
    expect(chip).toHaveTextContent("transcript f121381b");
    expect(chip).toHaveTextContent("hook");
  });

  it("says 未绑定 transcript when no deterministic channel binds", async () => {
    await renderSession(promotedInstance(), [lifecycle("transcript_unbound", {})]);
    const chip = screen.getByTestId("transcript-binding");
    expect(chip).toHaveAttribute("data-state", "unbound");
    expect(chip).toHaveTextContent("未绑定 transcript");
  });

  it("does not render the binding chip for an unpromoted terminal", async () => {
    const terminal = ttyLabInstance();
    await renderSession(
      { ...terminal, kind: "terminal", driver: "shell-pty", mode: "native", promotedAt: null },
      [lifecycle("transcript_unbound", {})],
    );
    expect(screen.queryByTestId("transcript-binding")).toBeNull();
  });
});
