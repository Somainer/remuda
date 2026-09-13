import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { SessionPage } from "./SessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
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
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [exited],
    events: { [exited.id]: [] },
  });
});
afterEach(() => vi.restoreAllMocks());

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
