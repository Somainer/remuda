import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Instance } from "../../types/instance";
import { known, unknownKnowledge, type Id } from "../../types/wire";

/**
 * ⑦ 行操作菜单 (home.md §9): the items a row's state allows, the 关闭… confirm
 * naming the session and the host, and the long-press entry.
 */

const calls = vi.hoisted(() => ({ markSeen: vi.fn().mockResolvedValue(null), cancel: vi.fn(), close: vi.fn(), rename: vi.fn() }));

vi.mock("../../lib/store", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/store")>();
  return {
    ...actual,
    useHub: () => ({ connection: "live" }),
    hubStore: {
      ...actual.hubStore,
      titleOf: (id: string) => `title ${id}`,
      hostName: () => "alpha",
      canInterrupt: true,
      markSeen: calls.markSeen,
      cancel: calls.cancel,
      close: calls.close,
      rename: calls.rename,
      toast: vi.fn(),
    },
  };
});

import { LONG_PRESS_MS, RowActions } from "./RowActions";

function inst(overrides: Partial<Instance> = {}): Instance {
  return {
    id: "ins_a" as Id,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    lifecycle: "running",
    activity: known("working"),
    connectivity: "connected",
    parent: null,
    cwd: "/work/proj",
    nativeRef: { sessionId: unknownKnowledge("hub") },
    capabilities: {},
    ...overrides,
  } as Instance;
}

function Harness({ instance }: { instance: Instance }) {
  const [open, setOpen] = useState(false);
  return (
    <MemoryRouter>
      <RowActions instance={instance} open={open} onOpen={() => setOpen(true)} onClose={() => setOpen(false)} />
    </MemoryRouter>
  );
}

function items() {
  return screen.getAllByRole("menuitem").map((item) => item.textContent);
}

describe("RowActions", () => {
  it("lists the items a running row allows, and not 标为已看", async () => {
    render(<Harness instance={inst()} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(items()).toEqual(["中断", "重命名", "复制目录", "在此项目新建", "关闭…"]);
  });

  it("lists 标为已看 for a done-unseen row and drops 中断", async () => {
    render(<Harness instance={inst({ activity: known("idle"), unseen: true })} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(items()).toContain("标为已看");
    expect(items()).not.toContain("中断");
  });

  it("drops 关闭… once the session has ended", async () => {
    render(<Harness instance={inst({ lifecycle: "exited", activity: known("idle") })} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(items()).not.toContain("关闭…");
    expect(items()).not.toContain("中断");
  });

  it("names the session and the host in the 关闭… confirm before closing", async () => {
    calls.close.mockClear();
    render(<Harness instance={inst()} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    await userEvent.click(screen.getByTestId("row-action-close"));
    const confirm = screen.getByTestId("row-close-confirm");
    expect(confirm).toHaveTextContent("title ins_a");
    expect(confirm).toHaveTextContent("alpha");
    expect(calls.close).not.toHaveBeenCalled();
    await userEvent.click(screen.getByTestId("row-close-confirm-yes"));
    expect(calls.close).toHaveBeenCalledWith("ins_a");
  });

  it("opens on a long-press and not on a short tap", async () => {
    render(<Harness instance={inst()} />);
    const wrap = screen.getByTestId("row-actions");
    const down = () => wrap.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, clientX: 5, clientY: 5 }));
    const up = () => wrap.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    down();
    await new Promise((resolve) => setTimeout(resolve, LONG_PRESS_MS - 50));
    up();
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(screen.queryByRole("menuitem")).toBeNull();
    down();
    await new Promise((resolve) => setTimeout(resolve, LONG_PRESS_MS + 50));
    expect(screen.getAllByRole("menuitem").length).toBeGreaterThan(0);
  });
});
