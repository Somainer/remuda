import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { Instance } from "../../types/instance";
import { known, unknownKnowledge, type Id } from "../../types/wire";

/**
 * ⑦ 行操作菜单 (home.md §9): the items a row's state allows, the 关闭… confirm
 * naming the session and the host, the long-press entry, and its teardown —
 * a dismissed menu must not re-open and a per-row mount must not touch the
 * shell's keyboard band.
 */

const calls = vi.hoisted(() => ({
  markSeen: vi.fn().mockResolvedValue(null),
  cancel: vi.fn().mockResolvedValue(undefined),
  close: vi.fn().mockResolvedValue(undefined),
  rename: vi.fn(),
  toast: vi.fn(),
}));

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
      toast: calls.toast,
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

function Harness({
  instance,
  stale = false,
  onOpen,
}: {
  instance: Instance;
  stale?: boolean;
  onOpen?: () => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <MemoryRouter>
      <RowActions
        instance={instance}
        open={open}
        onOpen={() => {
          onOpen?.();
          setOpen(true);
        }}
        onClose={() => setOpen(false)}
        stale={stale}
      />
    </MemoryRouter>
  );
}

function items() {
  return screen.getAllByRole("menuitem").map((item) => item.textContent);
}

function pressOnMore() {
  act(() => {
    screen.getByTestId("row-actions-more").dispatchEvent(
      new PointerEvent("pointerdown", { bubbles: true, clientX: 5, clientY: 5 }),
    );
  });
}

const wait = (ms: number) => act(() => new Promise((resolve) => setTimeout(resolve, ms)));

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

  it("refuses the portaled 关闭… confirm once the read is stale", async () => {
    calls.close.mockClear();
    const { rerender } = render(<Harness instance={inst()} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    await userEvent.click(screen.getByTestId("row-action-close"));
    expect(screen.getByTestId("row-close-confirm")).toBeTruthy();
    rerender(<Harness instance={inst()} stale />);
    const yes = screen.getByTestId("row-close-confirm-yes") as HTMLButtonElement;
    expect(yes.disabled).toBe(true);
    await userEvent.click(yes);
    expect(calls.close).not.toHaveBeenCalled();
  });

  it("opens on a long-press and not on a short tap", async () => {
    render(<Harness instance={inst()} />);
    const more = screen.getByTestId("row-actions-more");
    const down = () => act(() => {
      more.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, clientX: 5, clientY: 5 }));
    });
    down();
    await wait(LONG_PRESS_MS - 50);
    act(() => {
      more.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    });
    await wait(100);
    expect(screen.queryByRole("menuitem")).toBeNull();
    down();
    await wait(LONG_PRESS_MS + 50);
    expect(screen.getAllByRole("menuitem").length).toBeGreaterThan(0);
  });

  it("does not re-open 500ms after the backdrop dismisses the menu", async () => {
    const onOpen = vi.fn();
    render(<Harness instance={inst()} onOpen={onOpen} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    onOpen.mockClear();
    const panel = screen.getByTestId("row-actions-panel");
    const scrim = panel.parentElement!;
    // The scrim closes on mousedown; the pointerdown that precedes it must not
    // arm the long-press the wrapper used to own.
    act(() => {
      scrim.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, clientX: 5, clientY: 5 }));
      scrim.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    });
    expect(screen.queryByTestId("row-actions-panel")).toBeNull();
    await wait(LONG_PRESS_MS + 100);
    expect(onOpen).not.toHaveBeenCalled();
    expect(screen.queryByTestId("row-actions-panel")).toBeNull();
  });

  it("clears the press when the pointer is released anywhere", async () => {
    const onOpen = vi.fn();
    render(<Harness instance={inst()} onOpen={onOpen} />);
    pressOnMore();
    act(() => {
      window.dispatchEvent(new PointerEvent("pointerup"));
    });
    await wait(LONG_PRESS_MS + 100);
    expect(onOpen).not.toHaveBeenCalled();
    expect(screen.queryByTestId("row-actions-panel")).toBeNull();
  });

  it("clears the press when the row unmounts inside the hold window", async () => {
    const onOpen = vi.fn();
    const { unmount } = render(<Harness instance={inst()} onOpen={onOpen} />);
    pressOnMore();
    unmount();
    await wait(LONG_PRESS_MS + 100);
    expect(onOpen).not.toHaveBeenCalled();
  });

  it("leaves the shell's keyboard band alone when a row unmounts", () => {
    document.documentElement.style.setProperty("--workbench-height", "500px");
    document.documentElement.dataset.keyboard = "1";
    try {
      const { unmount } = render(<Harness instance={inst()} />);
      unmount();
      expect(document.documentElement.style.getPropertyValue("--workbench-height")).toBe("500px");
      expect(document.documentElement.dataset.keyboard).toBe("1");
    } finally {
      document.documentElement.style.removeProperty("--workbench-height");
      delete document.documentElement.dataset.keyboard;
    }
  });

  it("reports a refused 中断 instead of an unhandled rejection", async () => {
    calls.cancel.mockRejectedValueOnce(new Error("409"));
    calls.toast.mockClear();
    render(<Harness instance={inst()} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    await userEvent.click(screen.getByTestId("row-action-interrupt"));
    await vi.waitFor(() => expect(calls.toast).toHaveBeenCalledWith("中断失败，请重试"));
  });

  it("reports a refused 关闭 instead of an unhandled rejection", async () => {
    calls.close.mockRejectedValueOnce(new Error("410"));
    calls.toast.mockClear();
    render(<Harness instance={inst()} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    await userEvent.click(screen.getByTestId("row-action-close"));
    await userEvent.click(screen.getByTestId("row-close-confirm-yes"));
    await vi.waitFor(() => expect(calls.toast).toHaveBeenCalledWith("关闭失败，请重试"));
  });

  it("offers 复制恢复命令 only for a claude row", async () => {
    render(<Harness instance={inst({ nativeRef: { sessionId: known("sess_abc") } as Instance["nativeRef"] })} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(screen.getByTestId("row-action-copyResume").textContent).toBe("复制恢复命令");
  });

  it("has no 复制恢复命令 for a kind without a documented resume form", async () => {
    render(<Harness instance={inst({ kind: "codex", nativeRef: { sessionId: known("thr_1") } as Instance["nativeRef"] })} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(screen.queryByTestId("row-action-copyResume")).toBeNull();
  });

  it("drops 复制目录 for the workspace-id fallback", async () => {
    render(<Harness instance={inst({ cwd: "wsp_1" })} />);
    await userEvent.click(screen.getByTestId("row-actions-more"));
    expect(items()).not.toContain("复制目录");
  });
});
