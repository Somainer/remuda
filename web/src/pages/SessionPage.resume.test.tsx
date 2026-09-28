import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { SessionPage } from "./SessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
}));

// The page decides whether the strip is MOUNTED; what the strip draws for a
// given journal is LiveStatusStrip.test's concern.
vi.mock("../features/session/live/LiveStatusStrip", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../features/session/live/LiveStatusStrip")>()),
  LiveStatusStrip: () => <div data-testid="live-status-strip" />,
}));

// The ⋯ menu has no store subscription of its own, so its renders count the
// page body's renders.
const menuRenders = vi.hoisted(() => ({ count: 0 }));
vi.mock("../chrome/SessionMoreMenu", () => ({
  SessionMoreMenu: () => {
    menuRenders.count += 1;
    return null;
  },
}));

/** An exited claude-print session: no terminal, resume reported as supported. */
const exited = {
  ...mockDb.instances[0],
  id: "ins_exited" as typeof mockDb.instances[0]["id"],
  lifecycle: "exited" as const,
  activity: { state: "known", value: "idle" } as const,
  activeRunIds: [],
};

function renderPage() {
  return render(
    <MemoryRouter initialEntries={[`/s/${exited.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
        <Route path="/s/:instanceId/tty" element={<div data-testid="tty-route" />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [exited],
    events: { [exited.id]: [] },
  });
});
afterEach(() => vi.restoreAllMocks());

it("an emit that leaves this session's slice unchanged does not re-render the page body", () => {
  const snapshot = store.hubStore.getSnapshot();
  renderPage();
  const before = menuRenders.count;
  // The 2 s list refresh: freshly decoded copies of the same row, plus
  // host/workspace/other-session churn this page does not read.
  vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
    ...snapshot,
    instances: [structuredClone(exited), { ...exited, id: "ins_other" as typeof exited.id }],
    hosts: [],
    workspaces: [],
    events: { [exited.id]: snapshot.events[exited.id]!, ins_other: [] },
  });
  act(() => store.hubStore.toast("unrelated"));
  expect(menuRenders.count).toBe(before);
  // A real change to this session does re-render it.
  vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
    ...snapshot,
    instances: [{ ...exited, lastError: "native-exit-code-3" }],
  });
  act(() => store.hubStore.toast("changed"));
  expect(menuRenders.count).toBeGreaterThan(before);
});

it("offers both resume targets on an exited session", () => {
  renderPage();
  expect(screen.getByText("继续（结构化）")).toBeEnabled();
  expect(screen.getByTestId("resume-terminal")).toBeEnabled();
});

it("navigates to the new instance returned by a structured resume", async () => {
  const resume = vi.spyOn(store.hubStore, "resume").mockResolvedValue("ins_child" as never);
  renderPage();
  fireEvent.click(screen.getByText("继续（结构化）"));
  await waitFor(() => expect(resume).toHaveBeenCalledWith(exited.id, "structured"));
});

it("continues in a terminal by asking for the pty target and opening its tty view", async () => {
  const resume = vi.spyOn(store.hubStore, "resume").mockResolvedValue("ins_child" as never);
  renderPage();
  fireEvent.click(screen.getByTestId("resume-terminal"));
  await waitFor(() => expect(resume).toHaveBeenCalledWith(exited.id, "terminal"));
  await waitFor(() => expect(screen.getByTestId("tty-route")).toBeInTheDocument());
});

it("stays on the exited transcript when resume fails", async () => {
  // hubStore.resume reports the Hub's reason as a toast and returns null.
  const resume = vi.spyOn(store.hubStore, "resume").mockResolvedValue(null);
  renderPage();
  fireEvent.click(screen.getByText("继续（结构化）"));
  await waitFor(() => expect(resume).toHaveBeenCalled());
  expect(screen.getByTestId("session-page")).toBeInTheDocument();
});

it("replaces the composer with the ended bar and keeps the queued count visible", () => {
  vi.spyOn(store.hubStore, "heldBubbles").mockReturnValue([{}, {}] as never);
  renderPage();
  expect(screen.getByTestId("ended-bar")).toHaveTextContent("会话已结束");
  expect(screen.getByTestId("ended-held-note")).toHaveTextContent("有 2 条排队消息未送出");
  expect(screen.queryByTestId("composer")).toBeNull();
  expect(screen.getByTestId("resume-control")).toBeInTheDocument();
});

it("mounts no live strip beside the ended bar, but keeps it for a live session", () => {
  const { unmount } = renderPage();
  expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
  expect(screen.queryByTestId("live-status-strip")).toBeNull();
  unmount();
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [{ ...exited, lifecycle: "running" as const, connectivity: "connected" as const }],
    events: { [exited.id]: [] },
  });
  renderPage();
  expect(screen.queryByTestId("ended-bar")).toBeNull();
  expect(screen.getByTestId("live-status-strip")).toBeInTheDocument();
});

it("keeps the ended bar and resume for a disconnected node-restart row", () => {
  // The Hub marks a row ended by a Node restart disconnected as well; the
  // ended surface follows the durable lifecycle, not connectivity.
  const restarted = {
    ...exited,
    connectivity: "disconnected" as const,
    lastError: "node-epoch-changed",
  };
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [restarted],
    events: { [exited.id]: [] },
  });
  renderPage();
  expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
  expect(screen.getByTestId("node-restart-banner")).toHaveTextContent("Node 重启");
  expect(screen.getByTestId("node-restart-resume")).toBeEnabled();
  expect(screen.queryByTestId("composer")).toBeNull();
  expect(screen.queryByTestId("composer-input")).toBeNull();
  expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
  expect(screen.getByTestId("session-page")).toHaveAttribute("data-status", "exited");
});
