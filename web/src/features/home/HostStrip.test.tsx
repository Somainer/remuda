import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";
import type { HubState } from "../../lib/store";

const fake = vi.hoisted(() => ({
  state: null as unknown,
  freshness: { state: "fresh", lastOkAt: 1 as number | null, failingSince: null as number | null },
}));

vi.mock("../../lib/store", () => ({
  useHub: () => fake.state,
  hubStore: { titleOf: (id: string) => id },
}));

vi.mock("./freshness", () => ({
  useHomeFreshness: () => fake.freshness,
  HOME_FRESHNESS: { pollMs: 2_000, staleAfterMs: 10_000, bannerAfterMs: 5_000, receiptMs: 2_000 },
}));

vi.mock("../../lib/push", () => ({
  readPushStatus: async () => ({ permission: "granted", subscribed: true, endpoint: null, needsHomeScreen: false }),
}));

import { HostStrip } from "./HostStrip";

function hub(overrides: Partial<HubState> = {}): HubState {
  return { hosts: [], ...overrides } as unknown as HubState;
}

function renderStrip(ui = <HostStrip placement="desktop" trailing={<button data-testid="toggle">空间 ▸</button>} />) {
  return render(<MemoryRouter>{ui}</MemoryRouter>);
}

describe("② before the first snapshot", () => {
  it("keeps the band and its 「空间 ▸」 toggle, and shows 「–」 for the host chip", () => {
    fake.state = hub();
    fake.freshness = { state: "unknown", lastOkAt: null, failingSince: null };
    renderStrip();
    expect(screen.getByTestId("host-strip")).toBeInTheDocument();
    expect(screen.getByTestId("host-unknown")).toHaveTextContent("–");
    expect(screen.getByTestId("toggle")).toBeInTheDocument();
    // An unknown host list is not an empty one: no 添加主机 guess.
    expect(screen.queryByTestId("host-add")).toBeNull();
    expect(screen.queryByTestId("host-hub-chip")).toBeNull();
  });

  it("shows 添加主机 only once the snapshot says there are no hosts", () => {
    fake.state = hub();
    fake.freshness = { state: "fresh", lastOkAt: 1, failingSince: null };
    renderStrip();
    expect(screen.getByTestId("host-add")).toHaveTextContent("添加主机");
    expect(screen.queryByTestId("host-unknown")).toBeNull();
  });
});
