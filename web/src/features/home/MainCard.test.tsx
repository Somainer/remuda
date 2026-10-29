import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockDb } from "../../lib/mock";
import type { LineageRecord } from "../../lib/api";
import type { LocalBubble, MainEntry } from "../../lib/store";
import type { Instance } from "../../types/instance";
import { known, type Id } from "../../types/wire";
import { MainAskDock, MainCard } from "./MainCard";
import { resetMainRepliesForTest, TAIL_RETRY_MS } from "./mainTail";

const hub = {
  instances: [] as Instance[],
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
const objectUpload = vi.fn();

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
  return {
    ...actual,
    api: {
      ...actual.api,
      eventsRead: (...args: unknown[]) => eventsRead(...args),
      objectUpload: (...args: unknown[]) => objectUpload(...args),
    },
  };
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
  hub.instances = [];
  resetMainRepliesForTest();
  hub.connection = "live";
  main = { instance: chapter(), lineageId: "ins_main" as Id, generation: 1, live: true, chapterIds: ["ins_main" as Id] };
  send.mockReset().mockResolvedValue(true);
  localStorage.clear();
  globalThis.URL.createObjectURL = vi.fn(() => "blob:preview");
  globalThis.URL.revokeObjectURL = vi.fn();
  // Hub objects are bound to the instance they were uploaded under.
  objectUpload.mockReset().mockImplementation((instanceId: string, blob: Blob, mediaType: string, name?: string) =>
    Promise.resolve({ objectId: `obj_${instanceId}`, size: blob.size, kind: "file", name: name ?? null, mediaType }),
  );
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
    expect(send).toHaveBeenCalledWith("ins_g2", "还在吗", [], []);
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

function textFile(name = "notes.txt"): File {
  return new File(["hello"], name, { type: "text/plain" });
}

function pasteFiles(files: File[]) {
  return {
    clipboardData: {
      items: files.map((file) => ({ kind: "file", type: file.type, getAsFile: () => file })),
      files,
      types: ["Files"],
    },
  };
}

describe("问 Main across a chapter switch (r2 item 1)", () => {
  it("keeps the typed draft and the staged attachment when Resume moves Main to the next chapter", async () => {
    const user = userEvent.setup();
    main = { ...main!, instance: chapter({ lifecycle: "exited", lineageId: "ins_main" as Id }), live: false };
    const view = renderCard();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    await user.type(input, "draft stays");
    fireEvent.paste(input, pasteFiles([textFile()]));
    await waitFor(() => expect(objectUpload).toHaveBeenCalledTimes(1));
    expect(objectUpload.mock.calls[0][0]).toBe("ins_main");
    expect(await screen.findByTestId("attachment-chip")).toBeTruthy();

    // The resume lands: the resolution moves to chapter 2 of the SAME lineage.
    main = {
      instance: chapter({ id: "ins_g2" as Id, generation: 2, lineageId: "ins_main" as Id, chapterCause: "owner-resume" }),
      lineageId: "ins_main" as Id,
      generation: 2,
      live: true,
      chapterIds: ["ins_main" as Id, "ins_g2" as Id],
    };
    view.rerender(
      <MemoryRouter>
        <MainCard placement="desktop" />
      </MemoryRouter>,
    );
    const after = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    expect(after.value).toContain("draft stays");
    expect(screen.getByTestId("attachment-chip")).toBeTruthy();
    // The object is re-staged under the new chapter (Hub objects are bound to
    // their instance), then the send carries it there.
    await waitFor(() => expect(objectUpload).toHaveBeenCalledTimes(2));
    expect(objectUpload.mock.calls[1][0]).toBe("ins_g2");
    await waitFor(() => expect(screen.getByTestId("composer-send")).toBeEnabled());
    await user.click(screen.getByTestId("composer-send"));
    expect(send).toHaveBeenCalledTimes(1);
    const [target, text, refs] = send.mock.calls[0] as [string, string, { objectId: string }[]];
    expect(target).toBe("ins_g2");
    expect(text).toContain("draft stays");
    expect(refs.map((ref) => ref.objectId)).toEqual(["obj_ins_g2"]);
  });

  it("moves a draft left under an earlier chapter's id into the Main input", () => {
    localStorage.setItem("runtime.draft.ins_old", "left over");
    main = {
      instance: chapter({ id: "ins_cur" as Id, generation: 2, lineageId: "ins_old" as Id }),
      lineageId: "ins_old" as Id,
      generation: 2,
      live: true,
      chapterIds: ["ins_old" as Id, "ins_cur" as Id],
    };
    renderCard();
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).value).toBe("left over");
    expect(localStorage.getItem("runtime.draft.ins_old")).toBeNull();
  });
});

describe("line 2 when the tail cannot be read (r2 item 7)", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("an unreadable tail is not reported as 还没有回复, and the read is retried", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    eventsRead.mockReset().mockRejectedValueOnce(new Error("offline")).mockResolvedValue({
      events: [{ seq: "8", kind: "message", instanceId: "ins_main", payload: { role: "assistant", text: "回来了" } }],
      durableSeq: "9",
      windowFromSeq: "8",
      reachedAfterSeq: true,
    });
    renderCard();
    await waitFor(() => expect(eventsRead).toHaveBeenCalledTimes(1));
    await act(async () => undefined);
    const line2 = screen.getByTestId("main-card-reply");
    expect(line2.getAttribute("data-kind")).toBe("unread");
    expect(line2.textContent).not.toContain("还没有回复");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TAIL_RETRY_MS);
    });
    await waitFor(() => expect(screen.getByTestId("main-card-reply").textContent).toBe("回来了"));
    expect(eventsRead).toHaveBeenCalledTimes(2);
  });

  it("a read tail with no reply says so", async () => {
    eventsRead.mockReset().mockResolvedValue({ events: [], durableSeq: "9", windowFromSeq: null, reachedAfterSeq: true });
    renderCard();
    await waitFor(() => expect(screen.getByTestId("main-card-reply").getAttribute("data-kind")).toBe("empty"));
    expect(screen.getByTestId("main-card-reply").textContent).toBe("还没有回复");
  });
});

describe("the carried reply belongs to its lineage (r2 item 2)", () => {
  const page = (text: string | null) => ({
    events: text
      ? [{ seq: "8", kind: "message", instanceId: "x", payload: { role: "assistant", text } }]
      : [],
    durableSeq: "9",
    windowFromSeq: text ? "8" : null,
    reachedAfterSeq: true,
  });

  it("a new address-owner lineage never shows the old one's reply", async () => {
    eventsRead.mockReset().mockImplementation(({ journalId }: { journalId: string }) =>
      Promise.resolve(page(journalId === "jrn_main" ? "A 的回复" : null)),
    );
    const view = renderCard();
    await waitFor(() => expect(screen.getByTestId("main-card-reply").textContent).toBe("A 的回复"));
    // The old lineage ended; a NEW address-owner lineage is seated.
    main = {
      instance: chapter({ id: "ins_b1" as Id, journalId: "jrn_b1" as Id, lineageId: "ins_b1" as Id }),
      lineageId: "ins_b1" as Id,
      generation: 1,
      live: true,
      chapterIds: ["ins_b1" as Id],
    };
    view.rerender(
      <MemoryRouter>
        <MainCard placement="desktop" />
      </MemoryRouter>,
    );
    await waitFor(() => expect(screen.getByTestId("main-card-reply").getAttribute("data-kind")).toBe("empty"));
    expect(screen.getByTestId("main-card-reply").textContent).not.toContain("A 的回复");
  });

  it("after a reload, chapter N+1 with no reply yet shows chapter N's reply", async () => {
    const g1 = chapter({ id: "ins_g1" as Id, journalId: "jrn_g1" as Id, lineageId: "ins_g1" as Id, generation: 1, lifecycle: "exited" });
    const g2 = chapter({ id: "ins_g2" as Id, journalId: "jrn_g2" as Id, lineageId: "ins_g1" as Id, generation: 2 });
    hub.instances = [g1, g2];
    main = { instance: g2, lineageId: "ins_g1" as Id, generation: 2, live: true, chapterIds: ["ins_g1" as Id, "ins_g2" as Id] };
    eventsRead.mockReset().mockImplementation(({ journalId }: { journalId: string }) =>
      Promise.resolve(page(journalId === "jrn_g1" ? "第一章的回复" : null)),
    );
    renderCard();
    await waitFor(() => expect(screen.getByTestId("main-card-reply").textContent).toBe("第一章的回复"));
  });

  it("a remount keeps the carried reply without reading again", async () => {
    eventsRead.mockReset().mockImplementation(({ journalId }: { journalId: string }) =>
      Promise.resolve(page(journalId === "jrn_main" ? "记住我" : null)),
    );
    const first = renderCard();
    await waitFor(() => expect(screen.getByTestId("main-card-reply").textContent).toBe("记住我"));
    first.unmount();
    // Resume moved Main to chapter 2, whose tail has nothing yet.
    main = {
      instance: chapter({ id: "ins_g2" as Id, journalId: "jrn_g2" as Id, lineageId: "ins_main" as Id, generation: 2 }),
      lineageId: "ins_main" as Id,
      generation: 2,
      live: true,
      chapterIds: ["ins_main" as Id, "ins_g2" as Id],
    };
    renderCard();
    await waitFor(() => expect(eventsRead).toHaveBeenCalledWith(expect.objectContaining({ journalId: "jrn_g2" })));
    expect(screen.getByTestId("main-card-reply").textContent).toBe("记住我");
  });
});

describe("重试 keeps the message's attachments (r2 item 6)", () => {
  const attached = {
    objectId: "obj_shot",
    name: "shot.png",
    previewUrl: "blob:shot",
    kind: "image" as const,
    mediaType: "image/png",
    size: 4,
    index: 1,
  };

  it("重试 on the same chapter resends the text with the same attachment refs and previews", async () => {
    const user = userEvent.setup();
    hub.bubbles = [
      {
        clientRequestId: "loc_att" as Id,
        instanceId: "ins_main" as Id,
        text: "看图 [Image #1]",
        commandId: "cmd_att" as Id,
        state: "unknown",
        outboxState: "rejected",
        outboxError: "node busy",
        attachments: [attached],
        createdAt: "2026-10-30T11:00:00Z",
      },
    ];
    renderCard();
    await user.click(screen.getByTestId("main-card-retry"));
    expect(send).toHaveBeenCalledWith(
      "ins_main",
      "看图 [Image #1]",
      [{ index: 1, objectId: "obj_shot", kind: "image", mediaType: "image/png", name: "shot.png", size: 4 }],
      [attached],
    );
  });

  it("发送给当前 Main cannot carry another chapter's attachments: it is disabled and says why", async () => {
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
        clientRequestId: "loc_att2" as Id,
        instanceId: "ins_main" as Id,
        text: "看图 [Image #1]",
        commandId: "cmd_att2" as Id,
        state: "unknown",
        outboxState: "rejected",
        attachments: [attached],
        createdAt: "2026-10-30T11:00:00Z",
      },
    ];
    renderCard();
    const action = screen.getByTestId("main-card-send-current");
    expect(action).toBeDisabled();
    expect(screen.getByTestId("main-card-reply").textContent).toContain("附件");
    await user.click(action);
    expect(send).not.toHaveBeenCalled();
  });
});
