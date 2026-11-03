import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { Instance } from "../types/instance";
import { ttyLabInstance } from "../features/session/tty/fixture";
import { claimedCapabilities } from "../lib/capabilities";
import { SessionPage } from "./SessionPage";

/**
 * D-062 on /s/:id: a claimed external herdr pane carries the 「外部 herdr」
 * badge, its header button is 释放 (not Stop), and releasing it confirms
 * first with a sentence saying the pane keeps running.
 */
function claimedInstance(): Instance {
  const base = ttyLabInstance();
  return {
    ...base,
    kind: "claude",
    driver: "herdr-claimed",
    lifecycle: "ready",
    launchedBy: "user",
    capabilities: claimedCapabilities(),
    herdrClaim: { session: "work", workspaceId: "w2", tabId: "w2:t19", paneId: "w2:p5K", contact: "ok" },
  };
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

afterEach(() => {
  vi.restoreAllMocks();
});

async function renderSession(instance: Instance) {
  const store = await import("../lib/store");
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "titleOf").mockReturnValue("human claude");
  vi.spyOn(store.hubStore, "hostName").mockReturnValue("local");
  vi.spyOn(store.hubStore, "workspaceOf").mockReturnValue(undefined);
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [instance],
    events: { [instance.id]: [] },
    interactions: [],
    bubbles: [],
    hosts: [],
    journalStatus: {},
  } as ReturnType<typeof store.useHub>);
  const close = vi.spyOn(store.hubStore, "close").mockResolvedValue(undefined);
  render(
    <MemoryRouter initialEntries={[`/s/${instance.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/:view" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
  return { close };
}

describe("SessionPage for a claimed herdr pane", () => {
  it("shows the 外部 herdr badge and offers the terminal", async () => {
    await renderSession(claimedInstance());
    expect(screen.getByTestId("session-page")).toHaveAttribute("data-driver", "herdr-claimed");
    const badge = screen.getByTestId("session-herdr-claim-badge");
    expect(badge).toHaveTextContent("外部 herdr");
    expect(badge).toHaveTextContent("work · w2 · w2:t19");
  });

  it("shows the claimed pane's live viewport in the screen view", async () => {
    const { api } = await import("../lib/api");
    const read = vi
      .spyOn(api, "screenRead")
      .mockResolvedValue({ lines: ["HCLAIM-OK", "❯ "] } as Awaited<ReturnType<typeof api.screenRead>>);
    await renderSession(claimedInstance());
    expect(await screen.findByText(/HCLAIM-OK/)).toBeTruthy();
    expect(read).toHaveBeenCalledWith(claimedInstance().id, 80);
  });

  it("labels the header button 释放 and confirms before releasing", async () => {
    const { close } = await renderSession(claimedInstance());
    const button = screen.getByRole("button", { name: "Release" });
    expect(button).toHaveAttribute("title", "释放（pane 继续运行）");
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    await userEvent.click(button);
    const confirm = screen.getByTestId("session-release-confirm");
    expect(confirm).toHaveTextContent("释放「human claude」");
    expect(confirm).toHaveTextContent("继续运行");
    expect(close).not.toHaveBeenCalled();
    await userEvent.click(screen.getByTestId("session-release-confirm-yes"));
    expect(close).toHaveBeenCalledWith(claimedInstance().id);
  });
});
