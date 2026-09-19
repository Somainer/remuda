import { render, screen, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Id } from "../types/wire";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { HostDetailPage } from "./HostsPage";

/**
 * The host detail CLI table.
 *
 * The rows come from two sources: `installedCli` (what the Node found) and a
 * deliberately narrow absent list. The capability row is the only one that
 * gets an absent row — the Node probe reports *every* agent CLI it looked for,
 * so listing absent rows generally would give a claude-only host five empty
 * 未安装 rows for codex/grok/agy/gemini.
 */

/** A host whose Node looked for the agent CLIs and found only what is listed. */
function hostWithCli(
  cli: { kind: string; installed?: boolean; version?: string; path?: string }[],
): ReturnType<typeof store.hubStore.getSnapshot> {
  const base = mockDb.hosts[0];
  return {
    ...store.hubStore.getSnapshot(),
    hosts: [
      {
        ...base,
        id: "hst_fixture" as Id,
        label: "fixture",
        state: "online" as const,
        transport: { mode: "outbound-wss" as const, endpointRef: base.id },
        cli: cli.map((row) => ({ auth: "unknown" as const, ...row })),
      },
    ],
    workspaces: [],
    instances: [],
  };
}

function renderDetail() {
  render(
    <MemoryRouter initialEntries={["/hosts/hst_fixture"]}>
      <Routes>
        <Route path="/hosts/:hostId" element={<HostDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

/** The CLI table's rows, excluding the workspace/diagnostics sections. */
function cliRows(): HTMLElement[] {
  return screen.queryAllByTestId("host-cli");
}

beforeEach(() => {
  // The page polls hosts on mount; the test supplies the snapshot directly, so
  // the poll must not hit the network.
  vi.spyOn(store.hubStore, "refreshHosts").mockResolvedValue(undefined);
  // Install the spy first, so each case can set its own snapshot below.
  vi.spyOn(store, "useHub").mockReturnValue(store.hubStore.getSnapshot());
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("host detail CLI table", () => {
  it("renders one row for a claude-only host, not one per absent CLI", () => {
    // Every agent CLI the probe looked for comes back installed:false; only
    // claude is really there. The table must show exactly that one row.
    vi.mocked(store.useHub).mockReturnValue(
      hostWithCli([
        { kind: "claude", installed: true, version: "2.1.268", path: "/usr/bin/claude" },
        { kind: "codex", installed: false },
        { kind: "grok", installed: false },
        { kind: "agy", installed: false },
        { kind: "gemini", installed: false },
      ]),
    );
    renderDetail();
    expect(cliRows()).toHaveLength(1);
    expect(cliRows()[0]).toHaveTextContent("claude");
    expect(cliRows()[0]).toHaveTextContent("已安装");
  });

  it("renders the capability's absent row, and only the capability's", () => {
    vi.mocked(store.useHub).mockReturnValue(
      hostWithCli([
        { kind: "claude", installed: true, version: "2.1.268", path: "/usr/bin/claude" },
        { kind: "codex", installed: false },
        { kind: "computer-use", installed: false },
      ]),
    );
    renderDetail();
    // claude (installed) + computer-use (reported absent). codex is dropped.
    expect(cliRows()).toHaveLength(2);
    const absent = cliRows().filter((row) => within(row).queryByText("未安装"));
    expect(absent).toHaveLength(1);
    expect(absent[0]).toHaveTextContent("computer-use");
  });

  it("shows the placeholder when the only rows are non-capability absences", () => {
    // An older Node: it probes agent CLIs but not the capability, and none of
    // the CLIs it looked for are installed. Nothing is renderable — the absent
    // rows are non-capability ones, which the table deliberately does not draw
    // — so the guard must count the *rendered* set and show 尚无盘点 rather
    // than leaving an empty box.
    vi.mocked(store.useHub).mockReturnValue(
      hostWithCli([
        { kind: "claude", installed: false },
        { kind: "codex", installed: false },
        { kind: "grok", installed: false },
      ]),
    );
    renderDetail();
    expect(cliRows()).toHaveLength(0);
    expect(screen.getByText("尚无盘点")).toBeInTheDocument();
  });

  it("does not show the placeholder when only the capability's absent row renders", () => {
    // The complement: the capability absence *does* render, so the box is not
    // empty and the placeholder must stay away.
    vi.mocked(store.useHub).mockReturnValue(
      hostWithCli([{ kind: "computer-use", installed: false }]),
    );
    renderDetail();
    expect(cliRows()).toHaveLength(1);
    expect(screen.queryByText("尚无盘点")).not.toBeInTheDocument();
  });

  it("renders an installed capability row alongside the installed CLIs", () => {
    vi.mocked(store.useHub).mockReturnValue(
      hostWithCli([
        { kind: "claude", installed: true, version: "2.1.268", path: "/usr/bin/claude" },
        {
          kind: "computer-use",
          installed: true,
          version: "2.7.0",
          path: "/x/.codex/computer-use/SkyComputerUseClient",
        },
      ]),
    );
    renderDetail();
    expect(cliRows()).toHaveLength(2);
    const cap = cliRows().find((row) => row.textContent?.includes("computer-use"));
    expect(cap).toBeTruthy();
    expect(cap).toHaveTextContent("已安装");
    expect(cap).not.toHaveTextContent("未安装");
  });
});
