import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { INFO_DEBOUNCE_MS, notify, notifyStore, type NotifyInput } from "../lib/notify";
import { ShellNotify, Sidebar, shellChrome } from "./Shell";
import { PhoneNav } from "./PhoneNav";

afterEach(() => {
  notifyStore.reset();
  vi.useRealTimers();
});

/** `notify()` is a plain store call; React needs it flushed inside `act`. */
function post(input: NotifyInput): void {
  act(() => void notify(input));
}

/** Let the live-region debounce elapse. */
async function settle() {
  await waitFor(() => expect(screen.getByTestId("live-region").textContent).not.toBe(""), {
    timeout: INFO_DEBOUNCE_MS + 1000,
  });
}

describe("ShellNotify — live region hygiene (risk 4)", () => {
  it("exposes exactly one polite status region", () => {
    render(<ShellNotify />);
    const regions = screen.getAllByTestId("live-region");
    expect(regions).toHaveLength(1);
    expect(regions[0]).toHaveAttribute("role", "status");
    expect(regions[0]).toHaveAttribute("aria-live", "polite");
  });

  it("announces a confirmation as one short line", async () => {
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "保存", severity: "info" });
    await settle();
    expect(screen.getByTestId("live-region")).toHaveTextContent("会话 alpha · 保存");
  });

  it("the visible strip is aria-hidden, so a confirmation is announced once", async () => {
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "保存", severity: "info" });
    await settle();
    expect(screen.getByTestId("info-toasts")).toHaveAttribute("aria-hidden", "true");
  });

  it("never receives streaming transcript text", async () => {
    // The region renders `Notification.text` and nothing else. Streaming body
    // content reaches the DOM through Transcript, which is not wired to any
    // live region — assert the region ignores a busy transcript entirely.
    render(
      <>
        <ShellNotify />
        <div data-testid="transcript" aria-live="off">
          {"流式输出 ".repeat(50)}
        </div>
      </>,
    );

    post({ subject: "会话 alpha", stage: "保存", severity: "info" });
    await settle();

    const region = screen.getByTestId("live-region");
    expect(region).toHaveTextContent("会话 alpha · 保存");
    expect(region.textContent).not.toContain("流式输出");
    // And the transcript is explicitly opted out, so no ancestor can make it speak.
    expect(screen.getByTestId("transcript")).toHaveAttribute("aria-live", "off");
  });

  it("debounces a burst into a single announcement", async () => {
    vi.useFakeTimers();
    render(<ShellNotify />);

    // Five notifications inside one debounce window.
    for (let i = 0; i < 5; i++) post({ subject: `会话 ${i}`, stage: "保存", severity: "info" });

    const region = screen.getByTestId("live-region");
    // Nothing announced yet — the window has not closed.
    expect(region.textContent).toBe("");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(INFO_DEBOUNCE_MS + 50);
    });
    // Only the newest line is announced, not five.
    expect(region.textContent).toBe("会话 4 · 保存");
  });
});

describe("ShellNotify — blocking errors persist", () => {
  beforeEach(() => notifyStore.reset());

  it("renders object, stage and reason", () => {
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "删除", reason: "主机离线，数据待清理", severity: "blocking" });

    const error = screen.getByTestId("blocking-error");
    expect(error).toHaveTextContent("会话 alpha");
    expect(error).toHaveTextContent("删除");
    expect(error).toHaveTextContent("主机离线，数据待清理");
  });

  it("is a discoverable landmark rather than an anonymous div", () => {
    // P0-3 asks for a *discoverable* error area: a named region is how a
    // screen-reader user finds it without hunting the whole page.
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "删除", severity: "blocking" });
    expect(screen.getByRole("region", { name: "需要处理的问题" })).toBeInTheDocument();
  });

  it("is still visible after a later success notification", async () => {
    vi.useFakeTimers();
    render(<ShellNotify />);

    post({ subject: "会话 alpha", stage: "删除", reason: "主机离线", severity: "blocking" });
    post({ subject: "会话 beta", stage: "保存", severity: "info" });

    expect(screen.getByTestId("blocking-error")).toBeInTheDocument();

    // Long after the success has expired, the error is still there.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(screen.queryByTestId("info-toast")).not.toBeInTheDocument();
    expect(screen.getByTestId("blocking-error")).toHaveTextContent("会话 alpha");
  });

  it("goes away only on an explicit dismiss", async () => {
    const user = userEvent.setup();
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "删除", severity: "blocking" });

    await user.click(screen.getByTestId("blocking-dismiss"));
    expect(screen.queryByTestId("blocking-error")).not.toBeInTheDocument();
  });

  it("offers actions and runs them", async () => {
    const user = userEvent.setup();
    const refresh = vi.fn();
    render(<ShellNotify />);
    post({
      subject: "会话 alpha",
      stage: "发送",
      severity: "blocking",
      actions: [
        { id: "view", label: "查看", run: () => {} },
        { id: "refresh", label: "刷新", run: refresh },
      ],
    });

    expect(screen.getByTestId("blocking-action-view")).toHaveTextContent("查看");
    await user.click(screen.getByTestId("blocking-action-refresh"));
    expect(refresh).toHaveBeenCalledOnce();
  });

  it("copies only the sanitised diagnostic fields", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    // Must come after `setup()`, which installs a clipboard stub of its own.
    const user = userEvent.setup();
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });

    render(<ShellNotify />);
    post({
      subject: "会话 alpha",
      stage: "删除",
      reason: "主机离线",
      severity: "blocking",
      diagnostic: { instanceId: "ins_1", statusKey: "record-deleted-purge-pending" },
    });

    await user.click(screen.getByTestId("blocking-copy-diagnostic"));
    await waitFor(() => expect(writeText).toHaveBeenCalled());

    const copied = writeText.mock.calls[0][0] as string;
    expect(copied).toContain("instanceId: ins_1");
    expect(copied).toContain("statusKey: record-deleted-purge-pending");
    expect(copied).toContain("会话 alpha · 删除");
  });

  it("shows no copy button when there is no diagnostic to copy", () => {
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "删除", severity: "blocking" });
    expect(screen.queryByTestId("blocking-copy-diagnostic")).not.toBeInTheDocument();
  });

  it("stacks distinct errors and collapses repeats of the same one", () => {
    render(<ShellNotify />);
    post({ subject: "会话 alpha", stage: "删除", severity: "blocking" });
    post({ subject: "会话 beta", stage: "发送", severity: "blocking" });
    expect(screen.getAllByTestId("blocking-error")).toHaveLength(2);

    post({ subject: "会话 alpha", stage: "删除", reason: "重试后仍失败", severity: "blocking" });
    const errors = screen.getAllByTestId("blocking-error");
    expect(errors).toHaveLength(2);
    expect(screen.getByText("重试后仍失败")).toBeInTheDocument();
  });
});

function renderSidebar(path = "/sessions", props: Partial<Parameters<typeof Sidebar>[0]> = {}) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Sidebar collapsed={false} pending={0} newHref="/sessions/new" projects={[]} quickFindOwned={false} {...props} />
    </MemoryRouter>,
  );
}

describe("Sidebar (UO-2a)", () => {
  it("main nav leads with 会话, then 收件箱 and 任务看板", () => {
    renderSidebar();
    const links = within(screen.getByRole("navigation", { name: "主导航" })).getAllByRole("link");
    expect(links.map((link) => link.textContent)).toEqual(["会话", "收件箱", "任务看板"]);
    expect(links[0]).toHaveAttribute("href", "/sessions");
  });

  it("keeps 会话 current on a session page", () => {
    renderSidebar("/s/abc");
    expect(screen.getByRole("link", { name: "会话" })).toHaveAttribute("aria-current", "page");
  });

  it("reaches /fleet from the 管理 menu", async () => {
    renderSidebar("/board");
    await userEvent.click(screen.getByRole("button", { name: "管理" }));
    const menu = screen.getByRole("menu", { name: "管理" });
    expect(within(menu).getByRole("menuitem", { name: "集群" })).toHaveAttribute("href", "/fleet");
    expect(within(menu).getAllByRole("menuitem").map((item) => item.textContent)).toEqual(["主机", "集群", "Provider", "Bot", "设置"]);
  });

  it("lists 全局 then each project as sidebar-project-row", () => {
    renderSidebar("/board", { projects: [{ id: "p1", name: "web-app" }] });
    const rows = screen.getAllByTestId("sidebar-project-row");
    expect(rows.map((row) => row.textContent)).toEqual(["全局", "web-app"]);
    expect(rows[0]).toHaveAttribute("aria-pressed", "true");
  });

  it("keeps only the one 新建 title, and labels survive folding", () => {
    renderSidebar("/hosts", { collapsed: true });
    expect(screen.getAllByTitle("新建", { exact: true })).toHaveLength(1);
    expect(screen.getByTestId("sidebar")).toHaveAttribute("data-collapsed", "true");
    expect(screen.getByTestId("sidebar-toggle")).toHaveAccessibleName("展开侧栏");
    expect(screen.getByRole("link", { name: "收件箱" })).toBeInTheDocument();
  });

  it("leaves quickfind-trigger to the /sessions panel when it owns QuickFind", () => {
    const { unmount } = renderSidebar("/board");
    expect(screen.getAllByTestId("quickfind-trigger")).toHaveLength(1);
    unmount();
    renderSidebar("/sessions", { quickFindOwned: true });
    expect(screen.queryByTestId("quickfind-trigger")).toBeNull();
  });
});

describe("shellChrome (UO-2a)", () => {
  it("renders the tab strip on /s/* only", () => {
    expect(shellChrome("/s/abc", false).tabs).toBe(true);
    expect(shellChrome("/sessions", false).tabs).toBe(false);
    expect(shellChrome("/sessions/new", false).tabs).toBe(false);
    expect(shellChrome("/board", false).tabs).toBe(false);
  });

  it("never renders the phone home bar on /s/*", () => {
    expect(shellChrome("/s/abc", true).phoneNav).toBe(false);
    expect(shellChrome("/s/abc/structured", true).phoneNav).toBe(false);
    expect(shellChrome("/settings", true).phoneNav).toBe(true);
    expect(shellChrome("/settings", false).phoneNav).toBe(false);
  });
});

describe("PhoneNav (UO-2a)", () => {
  it("keeps the four phone-nav entries and the inbox badge", () => {
    render(
      <MemoryRouter initialEntries={["/settings"]}>
        <PhoneNav pending={3} newHref="/sessions/new" />
      </MemoryRouter>,
    );
    const bar = screen.getByRole("navigation", { name: "手机底栏" });
    expect(within(bar).getByTestId("phone-nav-home")).toHaveTextContent("会话");
    expect(within(bar).getByTestId("phone-nav-inbox")).toHaveAccessibleName("收件箱(3)");
    expect(within(bar).getByTestId("phone-inbox-badge")).toHaveTextContent("3");
    expect(within(bar).getByTestId("phone-nav-new")).toHaveAccessibleName("新建");
    expect(within(bar).getByTestId("phone-nav-more")).toHaveAttribute("data-active", "1");
  });
});
