import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import type { Instance } from "../types/instance";
import type { Id } from "../types/wire";
import { SessionPage } from "./SessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
  COMPACT_WORKBENCH_QUERY: "(max-width: 767px)",
}));

vi.mock("../features/session/live/LiveStatusStrip", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../features/session/live/LiveStatusStrip")>()),
  LiveStatusStrip: () => <div data-testid="live-status-strip" />,
}));

/** Chapter 1 of a Main lineage: ended and fenced by an owner resume. */
const chapter1 = {
  ...mockDb.instances[0],
  id: "ins_main_g1" as Id,
  lifecycle: "exited" as const,
  activity: { state: "known", value: "idle" } as const,
  activeRunIds: [],
  lineageId: "ins_main_g1" as Id,
  generation: 1,
  fencedAt: "2026-10-30T09:00:00Z",
  grants: ["address-owner"],
} as Instance;

/** Chapter 2: the live current chapter. */
const chapter2 = {
  ...chapter1,
  id: "ins_main_g2" as Id,
  lifecycle: "running" as const,
  connectivity: "connected" as const,
  generation: 2,
  chapterCause: "owner-resume",
  fencedAt: null,
} as Instance;

function renderPage(id: string) {
  return render(
    <MemoryRouter initialEntries={[`/s/${id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store.hubStore, "refreshLineage").mockResolvedValue(null);
  vi.spyOn(store.hubStore, "getSnapshot").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [chapter1, chapter2],
    events: { [chapter1.id]: [], [chapter2.id]: [] },
  });
});
afterEach(() => vi.restoreAllMocks());

it("an earlier Main chapter offers no resume, only a link to the current Main", () => {
  const resume = vi.spyOn(store.hubStore, "resume");
  renderPage(chapter1.id);
  expect(screen.getByTestId("ended-bar")).toBeInTheDocument();
  expect(screen.queryByTestId("resume-control")).toBeNull();
  expect(screen.queryByText("继续（结构化）")).toBeNull();
  expect(screen.queryByTestId("resume-terminal")).toBeNull();
  expect(screen.queryByTestId("main-resume")).toBeNull();
  const link = screen.getByTestId("ended-open-current-main");
  expect(link).toHaveTextContent("打开当前 Main");
  expect(link).toHaveAttribute("href", `/s/${chapter2.id}`);
  expect(resume).not.toHaveBeenCalled();
});

it("a plain ended session keeps its resume", () => {
  const plain = { ...chapter1, id: "ins_plain" as Id, lineageId: "ins_plain" as Id, grants: [], fencedAt: null } as Instance;
  vi.mocked(store.hubStore.getSnapshot).mockReturnValue({
    ...store.hubStore.getSnapshot(),
    instances: [plain, chapter2],
    events: { [plain.id]: [], [chapter2.id]: [] },
  });
  renderPage(plain.id);
  expect(screen.getByTestId("resume-control")).toBeInTheDocument();
  expect(screen.queryByTestId("ended-open-current-main")).toBeNull();
});
