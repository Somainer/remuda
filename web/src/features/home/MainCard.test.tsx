import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { LineageRecord } from "../../lib/api";
import type { LocalBubble, MainEntry } from "../../lib/store";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { MainAskDock, MainCard } from "./MainCard";

const hub = {
  hosts: [{ id: "host-a", label: "devbox", online: true, state: "online" }],
  interactions: [] as unknown[],
  bubbles: [] as LocalBubble[],
  summaries: {} as Record<string, string>,
  lineages: {} as Record<string, LineageRecord>,
  connection: "live",
};
let main: MainEntry | null = null;
const send = vi.fn();
const resume = vi.fn();
const reconcileSend = vi.fn();
const eventsRead = vi.fn();

vi.mock("../../lib/store", () => ({
  useHub: () => hub,
  useMainEntry: () => main,
  hubStore: {
    hostName: () => "devbox",
    send: (...args: unknown[]) => send(...args),
    resume: (...args: unknown[]) => resume(...args),
    reconcileSend: (...args: unknown[]) => reconcileSend(...args),
    refreshLineage: vi.fn().mockResolvedValue(null),
  },
}));

vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return { ...actual, api: { ...actual.api, eventsRead: (...args: unknown[]) => eventsRead(...args) } };
});

function chapter(patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id: "ins_main" as Id,
    hostId: "host-a" as Id,
    journalId: "jrn_main" as Id,
    durableSeq: "9",
    lifecycle: "running",
    connectivity: "connected",
    activity: known("idle"),
    grants: ["address-owner"],
    ...patch,
  } as Instance;
}

function renderCard(placement: "phone" | "desktop" = "desktop") {
  return render(
    <MemoryRouter>
      <MainCard placement={placement} />
    </MemoryRouter>,
  );
}

function renderPhone() {
  return render(
    <MemoryRouter>
      <MainCard placement="phone" />
      <MainAskDock />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  // jsdom has no matchMedia: a desktop, fine-pointer viewport.
  window.matchMedia = vi.fn().mockReturnValue({
    matches: false,
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
  }) as unknown as typeof window.matchMedia;
  hub.bubbles = [];
  hub.lineages = {};
  hub.connection = "live";
  main = { instance: chapter(), lineageId: "ins_main" as Id, generation: 1, live: true, chapterIds: ["ins_main" as Id] };
  send.mockReset().mockResolvedValue(true);
  resume.mockReset().mockResolvedValue("ins_g2");
  reconcileSend.mockReset().mockResolvedValue("sent");
  eventsRead.mockReset().mockResolvedValue({
    events: [{ seq: "8", kind: "message", instanceId: "ins_main", payload: { role: "assistant", text: "两项红\n细节" } }],
    durableSeq: "9",
    windowFromSeq: "8",
    reachedAfterSeq: true,
  });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("MainCard", () => {
  it("renders nothing without an address-owner lineage", () => {
    main = null;
    const { container } = renderCard();
    expect(container.querySelector("[data-testid='main-card']")).toBeNull();
  });

  it("line 1 is dot · Main · status, line 2 the first line of the last reply", async () => {
    renderCard();
    expect(screen.getByTestId("main-card-status").textContent).toMatch(/^空闲/);
    await waitFor(() => expect(screen.getByTestId("main-card-reply").textContent).toBe("两项红"));
    expect(screen.getByTestId("main-card-open").getAttribute("href")).toBe("/s/ins_main");
    // The compact input: no options trigger, the 问 Main placeholder.
    expect(screen.queryByTestId("model-effort-chip")).toBeNull();
    expect(screen.queryByTestId("composer-options-trigger")).toBeNull();
    expect(screen.getByTestId("composer-input").getAttribute("placeholder")).toBe("问 Main…  ⌘↵ 发送");
    // Desktop extras: chapter number and 展开 › to the Main session.
    expect(screen.getByTestId("main-card-chapter").textContent).toBe("第 1 章");
    expect(screen.getByTestId("main-card-expand").getAttribute("href")).toBe("/s/ins_main");
  });

  it("on the phone the card carries no input; the 问 Main input docks separately", () => {
    renderPhone();
    const card = screen.getByTestId("main-card");
    expect(card.querySelector("[data-testid='composer-input']")).toBeNull();
    const dock = screen.getByTestId("main-ask-dock");
    expect(dock.querySelector("[data-testid='composer-input']")).not.toBeNull();
  });

  it("renders no dock without an address-owner lineage", () => {
    main = null;
    const { container } = renderPhone();
    expect(container.querySelector("[data-testid='main-ask-dock']")).toBeNull();
  });

  it("sends to the resolved chapter through the store (D-055 outbox)", async () => {
    const user = userEvent.setup();
    renderCard();
    await user.type(screen.getByTestId("composer-input"), "看一下 gate");
    await user.click(screen.getByTestId("composer-send"));
    expect(send).toHaveBeenCalledWith("ins_main", "看一下 gate", [], []);
    // ⌘/Ctrl+Enter sends from the desktop input too.
    await user.type(screen.getByTestId("composer-input"), "第二条");
    await user.keyboard("{Control>}{Enter}{/Control}");
    expect(send).toHaveBeenLastCalledWith("ins_main", "第二条", [], []);
  });

  it("while paused the send button becomes 恢复 and resumes the chapter", async () => {
    const user = userEvent.setup();
    main = { ...main!, instance: chapter({ lifecycle: "exited" }), live: false };
    renderCard();
    expect(screen.getByTestId("main-card-status").textContent).toMatch(/^已停止/);
    expect(screen.queryByTestId("composer-send")).toBeNull();
    await user.type(screen.getByTestId("composer-input"), "draft stays");
    await user.click(screen.getByTestId("main-ask-resume"));
    expect(resume).toHaveBeenCalledWith("ins_main", "structured");
    expect(send).not.toHaveBeenCalled();
  });

  it("a superseded chapter's rejection offers 发送给当前 Main as a NEW send to the current chapter", async () => {
    const user = userEvent.setup();
    main = {
      instance: chapter({ id: "ins_g2" as Id, generation: 2, lineageId: "ins_main" as Id }),
      lineageId: "ins_main" as Id,
      generation: 2,
      live: true,
      chapterIds: ["ins_main" as Id, "ins_g2" as Id],
    };
    hub.bubbles = [
      {
        clientRequestId: "loc_1" as Id,
        instanceId: "ins_main" as Id,
        text: "还在吗",
        commandId: "cmd_old" as Id,
        state: "unknown",
        outboxState: "rejected",
        outboxError: "fenced",
        createdAt: "2026-10-30T11:00:00Z",
      },
    ];
    renderCard();
    expect(screen.getByTestId("main-card-reply").getAttribute("data-kind")).toBe("rejected");
    await user.click(screen.getByTestId("main-card-send-current"));
    expect(send).toHaveBeenCalledWith("ins_g2", "还在吗");
  });

  it("an unknown outcome is reconciled by one read, never resent", async () => {
    const user = userEvent.setup();
    hub.bubbles = [
      {
        clientRequestId: "loc_2" as Id,
        instanceId: "ins_main" as Id,
        text: "hello",
        commandId: "cmd_unknown" as Id,
        state: "unknown",
        outboxState: "unknown",
        createdAt: "2026-10-30T11:00:00Z",
      },
    ];
    renderCard();
    expect(screen.getByTestId("main-card-reply").textContent).toContain("状态待确认");
    await act(async () => {
      await user.click(screen.getByTestId("main-card-reconcile"));
    });
    expect(reconcileSend).toHaveBeenCalledWith("cmd_unknown");
    expect(send).not.toHaveBeenCalled();
  });

  it("host offline shows the queued-message placeholder", () => {
    hub.hosts = [{ id: "host-a", label: "devbox", online: false, state: "offline" }];
    renderCard();
    expect(screen.getByTestId("main-card-status").textContent).toMatch(/^主机 devbox 离线/);
    expect(screen.getByTestId("composer-input").getAttribute("placeholder")).toBe("主机离线 · 消息会排队，上线后送达");
    hub.hosts = [{ id: "host-a", label: "devbox", online: true, state: "online" }];
  });
});
