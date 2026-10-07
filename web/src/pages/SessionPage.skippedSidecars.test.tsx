/**
 * c-resumehome round 6 item 8: `relatedIds.skippedSidecars` renders as a
 * neutral notice in session run details — never as a session failure.
 */
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import type { Observation } from "../types/observation";
import { SessionPage } from "./SessionPage";

vi.mock("../lib/viewport", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/viewport")>()),
  useWorkbenchViewport: () => ({ mobile: false, offsetTop: 0 }),
  composing: () => false,
}));

const instance = mockDb.instances[0];

function skippedEvent(entries: string): Observation {
  return {
    eventId: "evt_skip_1",
    instanceId: instance.id,
    journalId: instance.id,
    seq: "9",
    kind: "lifecycle",
    observedAt: "2026-10-07T00:00:00Z",
    source: { channel: "runtime" },
    payload: {
      type: "native",
      topic: "diagnostic",
      nativeName: "resume_staging",
      nativeId: { state: "not-applicable" },
      status: { state: "known", value: "skipped-sidecars" },
      severity: "warning",
      affectsCompletion: false,
      dataRef: null,
      relatedIds: {
        severity: "warning",
        skippedSidecars: entries,
      },
    },
  } as unknown as Observation;
}

function renderWithEvents(events: Observation[]) {
  vi.spyOn(store.hubStore, "follow").mockResolvedValue(undefined);
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(),
    ready: true,
    instances: [instance],
    events: { [instance.id]: events },
  });
  return render(
    <MemoryRouter initialEntries={[`/s/${instance.id}/structured`]}>
      <Routes>
        <Route path="/s/:instanceId/structured" element={<SessionPage view="structured" />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  localStorage.clear();
});
afterEach(() => {
  vi.restoreAllMocks();
});

it("renders skipped sidecars as a non-error notice with the raw entries", async () => {
  renderWithEvents([
    skippedEvent("symlink:S/subagents/evil.jsonl,non-regular:S/sock"),
  ]);
  const chip = await waitFor(() =>
    screen.getByTestId("run-details-skipped-sidecars"),
  );
  expect(chip.getAttribute("data-error")).not.toBe("1");
  expect(chip.getAttribute("data-entries")).toBe(
    "symlink:S/subagents/evil.jsonl,non-regular:S/sock",
  );
  expect(chip.textContent).toContain("侧车");
  expect(chip.textContent).toContain("不影响");
});

it("renders no notice when the window has no skipped-sidecar diagnostic", () => {
  renderWithEvents([]);
  expect(screen.queryByTestId("run-details-skipped-sidecars")).toBeNull();
});
