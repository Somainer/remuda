import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Link, MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { HubHttpError } from "../lib/httpError";
import * as store from "../lib/store";
import { notifyStore } from "../lib/notify";
import { clearNewSessionDraft } from "../lib/newSessionDraft";
import { mockDb } from "../lib/mock";
import { NewSessionPage } from "./NewSessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false }), composing: () => false,
}));

const host = { ...mockDb.hosts[0], state: "online" as const };
const workspace = { ...mockDb.workspaces[0], id: "wsp_project", hostId: host.id, rootPath: "/home/dev/projects/app" };

function Probe() {
  const location = useLocation();
  return (
    <div>
      <div data-testid="probe">{location.pathname}</div>
      <Link to="/sessions/new" data-testid="open-new">新建</Link>
    </div>
  );
}

/** Render the page inside real routes so origin-return can be observed. */
function renderAt(entries: string[]) {
  return render(
    <MemoryRouter initialEntries={entries} initialIndex={entries.length - 1}>
      <Routes>
        <Route path="/sessions/new" element={<NewSessionPage />} />
        <Route path="/sessions" element={<Probe />} />
        <Route path="/s/:id" element={<Probe />} />
      </Routes>
    </MemoryRouter>,
  );
}

/** Open New Session the way the app does — an in-app PUSH from an origin. */
function openFrom(origin: string) {
  renderAt([origin]);
  fireEvent.click(screen.getByTestId("open-new"));
}

beforeEach(() => {
  localStorage.clear();
  // Drafts have a memory-only mode (no session in these tests); the memory map
  // is module state, so clear the context the fixtures use between cases.
  clearNewSessionDraft(null, host.id, workspace.id);
  vi.spyOn(api, "providerList").mockResolvedValue({ items: [] });
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [host], workspaces: [workspace], instances: [],
  });
});
afterEach(() => {
  vi.restoreAllMocks();
  notifyStore.reset();
});

it("creates with the registered ID and a cwd inside the chosen workspace", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderAt(["/sessions/new"]);
  expect(screen.getByTestId("new-session-cwd")).toHaveValue("");
  fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "src" } });
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    hostId: host.id, workspaceId: workspace.id, cwd: "/home/dev/projects/app/src",
  })));
});

it("blocks absolute or escaping cwd text instead of sending it to another root", () => {
  renderAt(["/sessions/new"]);
  fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "~" } });
  expect(screen.getByRole("alert")).toHaveTextContent("相对子路径");
  expect(screen.getByTestId("new-session-start")).toBeDisabled();
});

it("creates a worktree in the selected project and uses its returned path", async () => {
  const createTree = vi.spyOn(store.hubStore, "createWorktree").mockResolvedValue({ name: "feature", path: "/home/dev/projects/app-wt/feature" });
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderAt(["/sessions/new"]);
  fireEvent.click(screen.getByTestId("cwd-mode-worktree"));
  expect(screen.getByTestId("new-session-workspace")).toHaveValue(workspace.id);
  fireEvent.change(screen.getByTestId("new-session-worktree-name"), { target: { value: "feature" } });
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    hostId: host.id, workspaceId: workspace.id, cwd: "/home/dev/projects/app-wt/feature", worktree: "feature",
  })));
  expect(createTree).toHaveBeenCalledWith({ hostId: host.id, workspaceId: workspace.id, name: "feature", base: "main" });
});

it("does not invent a root for a host with no registered workspace", () => {
  vi.mocked(store.useHub).mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [host], workspaces: [], instances: [],
  });
  renderAt(["/sessions/new"]);
  expect(screen.getByTestId("new-session-start")).toBeDisabled();
  expect(screen.getByTestId("new-session-workspace")).toHaveTextContent("添加目录");
  expect(screen.getByTestId("workspace-add")).toBeEnabled();
});

/** The harness picker and the effort slider share one selection: switching re-snaps it. */
const cliHost = {
  ...host,
  cli: [
    { kind: "claude", version: "2.1.0", path: "/usr/bin/claude" },
    { kind: "codex", version: "0.9.0", path: "/usr/bin/codex" },
    { kind: "grok", version: "0.4.0", path: "/usr/bin/grok" },
  ],
};

function renderWithCli(entries = ["/sessions/new"]) {
  vi.mocked(store.useHub).mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [cliHost], workspaces: [workspace], instances: [],
  });
  renderAt(entries);
}

/** A host whose Node reports that it can launch agents inside its own PTY (D-028 P2 core). */
function renderWithLaunchableMatrix() {
  const launchable = {
    ...cliHost,
    capabilities: { ...(cliHost as { capabilities?: object }).capabilities, driverInventory: [{ kind: "shell-pty", launchable: true }] },
  };
  vi.mocked(store.useHub).mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [launchable], workspaces: [workspace], instances: [],
  });
  renderAt(["/sessions/new"]);
}

it("mounts the inline slider (layout A, no card) with the six Claude stops", () => {
  renderWithCli();
  const slider = screen.getByTestId("new-session-effort-slider");
  expect(slider).toHaveAttribute("role", "slider");
  expect(slider).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultracode");
  expect(slider).toHaveAttribute("aria-valuemax", "5");
  // The device default is Claude `high`, the third of six stops.
  expect(slider).toHaveAttribute("data-name", "high");
  expect(slider).toHaveAttribute("data-index", "2");
  expect(screen.getByTestId("new-session-effort-knob")).toBeInTheDocument();
  expect(screen.getByTestId("new-session-effort-track")).toBeInTheDocument();
  // Layout A: the label row and tick labels are present, with no card frame.
  expect(screen.getByTestId("new-session-effort-slider-panel")).toHaveAttribute("data-variant", "inline");
  // The helper stays user vocabulary; the InstanceSpec implementation name no
  // longer appears anywhere on the New Session form.
  expect(screen.getByTestId("new-session-effort-foot")).toHaveTextContent("会话开始后仍可在会话内调整");
  expect(screen.getByTestId("new-session-effort")).toHaveTextContent("会话开始后仍可在会话内调整");
  // The standalone ultracode chip is gone; ultracode is the last tick.
  expect(screen.queryByTestId("new-session-effort-ultracode")).toBeNull();
});

it("re-snaps the slider onto the new harness table when the runtime changes", () => {
  renderWithCli();
  const slider = () => screen.getByTestId("new-session-effort-slider");
  // Settings default: claude `high` at the midpoint.
  expect(slider()).toHaveAttribute("data-name", "high");
  expect(slider()).toHaveAttribute("data-index", "2");

  // Codex (six native stops): the midpoint maps onto Extra high by nearest position.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-tiers", "low,medium,high,xhigh,max,ultra");
  expect(slider()).toHaveAttribute("data-name", "xhigh");
  expect(slider()).toHaveAttribute("data-index", "3");
  // Ultra is a native Codex tier; the workflow flag remains Claude-only.
  expect(slider()).toHaveAttribute("aria-valuemax", "5");
  expect(slider()).toHaveAttribute("data-ultracode", "0");

  // Grok has four stops; the Codex position (3/5) snaps onto high.
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-tiers", "low,medium,high,xhigh");
  expect(slider()).toHaveAttribute("data-name", "high");
  expect(slider()).toHaveAttribute("data-index", "2");
  expect(slider()).toHaveAttribute("data-ember", "0");
});

it("keeps the top tier on top across harnesses and drops the draft with it", () => {
  renderWithCli();
  const slider = () => screen.getByTestId("new-session-effort-slider");
  slider().focus();
  fireEvent.keyDown(slider(), { key: "End" });
  // End lands on the sixth stop, ultracode (xhigh tier + workflow flag).
  expect(slider()).toHaveAttribute("data-name", "ultracode");
  expect(slider()).toHaveAttribute("data-index", "5");
  expect(slider()).toHaveAttribute("data-ember", "1");

  // The ultracode flag is Claude-only; leaving Claude drops it and maps the
  // xhigh tier (3/4) by ratio onto grok `high` (2/3) — top accents map to the
  // static accent, the ember never crosses harnesses.
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "high");
  expect(slider()).toHaveAttribute("data-index", "2");
  expect(slider()).toHaveAttribute("data-effort-look", "plain");
  expect(slider()).toHaveAttribute("data-ember", "0");

  // ...and back, without the stale draft the unmounted card held.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-name", "xhigh");
  expect(slider()).toHaveAttribute("data-index", "3");
  fireEvent.keyDown(slider(), { key: "Home" });
  expect(slider()).toHaveAttribute("data-name", "low");
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "low");
  expect(slider()).toHaveAttribute("data-ember", "0");
});

it("the ultracode stop is reached on the one slider and the create carries the ultracode wire name", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderWithCli();
  const slider = () => screen.getByTestId("new-session-effort-slider");
  // End walks all six stops to ultracode; the track stays an enabled slider.
  slider().focus();
  fireEvent.keyDown(slider(), { key: "End" });
  expect(slider()).toHaveAttribute("data-name", "ultracode");
  expect(slider()).toHaveAttribute("data-index", "5");
  expect(slider()).toHaveAttribute("data-tier-index", "3");
  expect(slider()).toHaveAttribute("data-ultracode", "1");
  expect(slider()).toHaveAttribute("data-ember", "1");
  expect(slider()).toHaveAttribute("aria-disabled", "false");
  expect(screen.getByTestId("new-session-effort-title")).toHaveTextContent("ultracode");
  // One step back returns to max on the same slider.
  fireEvent.keyDown(slider(), { key: "ArrowLeft" });
  expect(slider()).toHaveAttribute("data-name", "max");
  expect(slider()).toHaveAttribute("data-index", "4");
  fireEvent.keyDown(slider(), { key: "ArrowRight" });
  expect(slider()).toHaveAttribute("data-name", "ultracode");

  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() =>
    expect(create).toHaveBeenCalledWith(
      expect.objectContaining({ kind: "claude", effortIndex: 3, effortName: "ultracode" }),
    ),
  );
});

it("writes the slider's tier into the instance it creates", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderWithCli();
  const slider = screen.getByTestId("new-session-effort-slider");
  // Start on high (index 2); one step right is xhigh.
  fireEvent.keyDown(slider, { key: "ArrowRight" });
  expect(slider).toHaveAttribute("data-name", "xhigh");
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    kind: "claude", effortIndex: 3, effortName: "xhigh",
  })));
});

it("normalizes a remembered legacy tier name when the sheet opens", () => {
  localStorage.setItem(
    "runtime.new-session",
    JSON.stringify({ effortIndex: 3, effortName: "think-hard" }),
  );
  renderWithCli();
  const slider = screen.getByTestId("new-session-effort-slider");
  // think-hard → xhigh (index 3), no ultracode.
  expect(slider).toHaveAttribute("data-name", "xhigh");
  expect(slider).toHaveAttribute("data-index", "3");
  expect(slider).toHaveAttribute("data-ultracode", "0");
});

it("writes the harness-native tier after a runtime switch", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderWithCli();
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  fireEvent.keyDown(screen.getByTestId("new-session-effort-slider"), { key: "End" });
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    kind: "grok", effortIndex: 3, effortName: "xhigh",
  })));
});

describe("return to origin", () => {
  it("goes back to the session it was opened from instead of always the list", () => {
    openFrom("/s/inst_origin");
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(screen.getByTestId("probe")).toHaveTextContent("/s/inst_origin");
  });

  it("the header close button and Escape return to the origin too", () => {
    openFrom("/sessions");
    fireEvent.keyDown(screen.getByTestId("new-session-sheet"), { key: "Escape" });
    expect(screen.getByTestId("probe")).toHaveTextContent("/sessions");
  });

  it("falls back to the list for a fresh deep link with no in-app origin", () => {
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(screen.getByTestId("probe")).toHaveTextContent("/sessions");
  });
});

describe("drafts", () => {
  it("keeps the draft when closing with Escape and restores it on reopen", async () => {
    openFrom("/s/inst_origin");
    fireEvent.change(screen.getByTestId("new-session-prompt"), { target: { value: "查一半的事" } });
    fireEvent.keyDown(screen.getByTestId("new-session-sheet"), { key: "Escape" });
    expect(screen.getByTestId("probe")).toHaveTextContent("/s/inst_origin");

    // Reopen the deep link (same tab): the body is restored for this context.
    renderAt(["/sessions/new"]);
    await waitFor(() => expect(screen.getByTestId("new-session-prompt")).toHaveValue("查一半的事"));
  });

  it("explicit 丢弃草稿 clears it before returning", async () => {
    openFrom("/s/inst_origin");
    fireEvent.change(screen.getByTestId("new-session-prompt"), { target: { value: "不要了" } });
    fireEvent.click(screen.getByTestId("new-session-discard"));
    expect(screen.getByTestId("probe")).toHaveTextContent("/s/inst_origin");

    renderAt(["/sessions/new"]);
    await waitFor(() => expect(screen.getByTestId("new-session-prompt")).toHaveValue(""));
  });

  it("does not bleed a body into a workspace that has no draft of its own", async () => {
    const second = { ...workspace, id: "wsp_other", rootPath: "/srv/other" };
    vi.mocked(store.useHub).mockReturnValue({
      ...store.hubStore.getSnapshot(), hosts: [host], workspaces: [workspace, second], instances: [],
    });
    renderAt(["/sessions/new"]);
    fireEvent.change(screen.getByTestId("new-session-prompt"), { target: { value: "只属于第一个目录" } });
    await waitFor(() => expect(screen.getByTestId("new-session-prompt")).toHaveValue("只属于第一个目录"));

    // Same host, different registered workspace: no draft there, so the body
    // must not follow the selection.
    fireEvent.change(screen.getByTestId("new-session-workspace"), { target: { value: second.id } });
    await waitFor(() => expect(screen.getByTestId("new-session-prompt")).toHaveValue(""));

    // Switching back restores the first context's own draft.
    fireEvent.change(screen.getByTestId("new-session-workspace"), { target: { value: workspace.id } });
    await waitFor(() => expect(screen.getByTestId("new-session-prompt")).toHaveValue("只属于第一个目录"));
  });
});

describe("first layer uses user vocabulary", () => {
  const IMPLEMENTATION_WORDS = [
    "InstanceSpec",
    "driver",
    "Driver",
    "carrier",
    "shell-pty",
    "claude-print",
    "claude-pty",
    "generic-pty",
    "PTY",
    "provider",
    "Provider",
  ];

  it("hides carrier and implementation ids behind 高级设置 on the default claude form", () => {
    renderWithCli();
    const sheet = screen.getByTestId("new-session-sheet");
    // User-facing first layer: what / where / which agent / permission.
    expect(sheet).toHaveTextContent("要做什么");
    expect(sheet).toHaveTextContent("工作目录");
    expect(sheet).toHaveTextContent("执行 agent");
    expect(sheet).toHaveTextContent("权限");
    expect(sheet).toHaveTextContent("模型来源");
    for (const word of IMPLEMENTATION_WORDS) {
      expect(sheet.textContent).not.toContain(word);
    }
    // The driver matrix moves into 高级设置 and is reachable from there.
    expect(screen.queryByTestId("new-session-driver-row")).toBeNull();
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    expect(screen.getByTestId("new-session-driver-row")).toBeInTheDocument();
    expect(sheet.textContent).toContain("shell-pty");
  });

  it("names the one terminal carrier on the terminal surface", () => {
    renderWithCli();
    fireEvent.click(screen.getByTestId("new-session-kind-terminal"));
    expect(screen.getByTestId("new-session-terminal-driver")).toHaveTextContent("shell-pty");
  });
});

describe("idempotent submit when the ACK is unknown", () => {
  it("shows 状态待确认 and never issues a second create", async () => {
    const create = vi
      .spyOn(store.hubStore, "create")
      .mockRejectedValue(new TypeError("network error: response lost"));
    renderAt(["/sessions/new"]);
    fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "src" } });
    fireEvent.click(screen.getByTestId("new-session-start"));

    const unknown = await screen.findByTestId("new-session-unknown");
    expect(unknown).toHaveAttribute("role", "status");
    expect(unknown).toHaveTextContent("状态待确认");
    // The same fact goes to the standing blocking region (plan §2 notify
    // contract) so it survives navigation to the list.
    const blocking = notifyStore.getState().blocking;
    expect(blocking).toHaveLength(1);
    expect(blocking[0].severity).toBe("blocking");
    expect(blocking[0].stage).toBe("状态待确认");
    expect(blocking[0].diagnostic?.statusKey).toBe("unconfirmed");
    // One attempt only...
    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    // ...and the primary action can not start a second one.
    expect(screen.getByTestId("new-session-start")).toBeDisabled();
    const form = screen.getByTestId("new-session-sheet").querySelector("form")!;
    fireEvent.submit(form);
    await Promise.resolve();
    expect(create).toHaveBeenCalledTimes(1);
    // The client request id is shown so the attempt can be reconciled manually.
    expect(screen.getByTestId("new-session-client-request-id").textContent).toMatch(/^creq_/);
  });

  it("treats a definite 4xx refusal as a fixable error and allows one corrected submit", async () => {
    const create = vi
      .spyOn(store.hubStore, "create")
      .mockRejectedValueOnce(new HubHttpError(422, "PLACEMENT_UNSATISFIABLE", "主机没有空闲实例槽位"))
      .mockResolvedValueOnce(mockDb.instances[0]);
    renderAt(["/sessions/new"]);
    fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "src" } });
    fireEvent.click(screen.getByTestId("new-session-start"));
    const error = await screen.findByTestId("new-session-error");
    expect(error).toHaveTextContent("主机没有空闲实例槽位");
    expect(screen.queryByTestId("new-session-unknown")).toBeNull();
    // A fixable refusal stays inline; it does not post to the standing region.
    expect(notifyStore.getState().blocking).toHaveLength(0);
    expect(screen.getByTestId("new-session-start")).toBeEnabled();
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalledTimes(2));
  });
});

describe("D-028 native PTY default", () => {
  it("offers 原生终端 shell-pty as the default for claude and creates with it", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderWithLaunchableMatrix();
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    const row = screen.getByTestId("new-session-driver-shell-pty");
    expect(row).toHaveAttribute("data-default", "1");
    expect(screen.getByTestId("new-session-launch-preview")).toHaveTextContent(/claude/);
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() =>
      expect(create).toHaveBeenCalledWith(expect.objectContaining({ kind: "claude", driver: "shell-pty" })),
    );
  });

  it("keeps claude-print selectable as a secondary choice", () => {
    renderWithLaunchableMatrix();
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    const print = screen.getByTestId("new-session-driver-claude-print");
    expect(print).toHaveAttribute("data-default", "0");
    fireEvent.click(print);
    expect(print.className).toMatch(/driverChoiceOn/);
  });

  it("falls back to claude-pty when the host's matrix says shell-pty is not launchable", () => {
    vi.mocked(store.useHub).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      hosts: [
        {
          ...cliHost,
          capabilities: { driverInventory: [{ kind: "shell-pty", launchable: false }] },
        },
      ],
      workspaces: [workspace],
      instances: [],
    });
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    const shell = screen.getByTestId("new-session-driver-shell-pty");
    expect(shell).toBeDisabled();
    expect(shell).toHaveAttribute("data-default", "0");
    expect(screen.getByTestId("new-session-driver-claude-pty")).toHaveAttribute("data-default", "1");
  });

  it("falls back to generic-pty for grok when the installed CLI meets a matrix refusing shell-pty", () => {
    vi.mocked(store.useHub).mockReturnValue({
      ...store.hubStore.getSnapshot(),
      hosts: [
        {
          ...cliHost,
          capabilities: { driverInventory: [{ kind: "shell-pty", launchable: false }] },
        },
      ],
      workspaces: [workspace],
      instances: [],
    });
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-kind-grok"));
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    expect(screen.getByTestId("new-session-driver-generic-pty")).toHaveAttribute("data-default", "1");
    expect(screen.getByTestId("new-session-driver-shell-pty")).toBeDisabled();
  });
});

describe("特殊参数 and claude 可执行文件", () => {
  it("splits args on whitespace into an argv array rather than sending a shell string", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    fireEvent.change(screen.getByTestId("new-session-args"), {
      target: { value: "  --effort   high --ide " },
    });
    fireEvent.change(screen.getByTestId("new-session-binary"), {
      target: { value: " /opt/claude/bin/claude " },
    });
    // The chips are the argv the Hub will receive, so a user can see that
    // runs of whitespace collapse and nothing is shell-parsed.
    expect(screen.getByTestId("new-session-args-chips")).toHaveTextContent("--effort");
    expect(screen.getByTestId("new-session-args-chips")).toHaveTextContent("--ide");
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() =>
      expect(create).toHaveBeenCalledWith(
        expect.objectContaining({
          args: ["--effort", "high", "--ide"],
          binaryPath: "/opt/claude/bin/claude",
        }),
      ),
    );
  });

  it("omits both fields when they are blank so the host default still applies", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    fireEvent.change(screen.getByTestId("new-session-args"), { target: { value: "   " } });
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalled());
    const spec = create.mock.calls[0][0] as { args?: string[]; binaryPath?: string };
    // Sending `[]` would replace the host default with "no args", which is a
    // different request from "I did not choose".
    expect(spec.args).toBeUndefined();
    expect(spec.binaryPath).toBeUndefined();
  });

  it("remembers args across sessions but never the executable", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    fireEvent.change(screen.getByTestId("new-session-args"), { target: { value: "--effort high" } });
    fireEvent.change(screen.getByTestId("new-session-binary"), {
      target: { value: "/opt/claude/bin/claude" },
    });
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalled());
    const stored = localStorage.getItem("runtime.new-session") ?? "";
    expect(stored).toContain("--effort high");
    // A silently restored executable is the kind of thing you would not
    // think to check before starting a run.
    expect(stored).not.toContain("/opt/claude/bin/claude");
  });

  it("shows the host default as a placeholder instead of prefilling the field", () => {
    vi.spyOn(store, "useHub").mockReturnValue({
      ...store.hubStore.getSnapshot(),
      hosts: [{ ...host, defaultLaunchArgs: ["--effort", "max"], claudeBinaryPath: "/srv/claude" }],
      workspaces: [workspace],
      instances: [],
    });
    renderAt(["/sessions/new"]);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    // Typing over a prefilled default would turn an operator's host-level
    // choice into a session value, and the Hub merges the two differently.
    expect(screen.getByTestId("new-session-args")).toHaveValue("");
    expect(screen.getByTestId("new-session-args")).toHaveAttribute("placeholder", "--effort max");
    expect(screen.getByTestId("new-session-binary")).toHaveValue("");
    expect(screen.getByTestId("new-session-binary")).toHaveAttribute("placeholder", "/srv/claude");
  });
});


describe("Claude terminal renderer", () => {
  it("offers both renderers with the in-session switch helper and submits the selection", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    const control = screen.getByTestId("new-session-tui");
    expect(control).toHaveValue("fullscreen");
    expect(screen.getByRole("option", { name: "全屏渲染（推荐）" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "行内渲染" })).toBeInTheDocument();
    expect(control).toHaveAccessibleDescription("启动时使用此渲染方式，会话内可用 /tui 切换");
    fireEvent.change(control, { target: { value: "default" } });
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({ tui: "default" })));
  });

  it("shows the host default without converting it into a session override", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    vi.mocked(store.useHub).mockReturnValue({
      ...store.hubStore.getSnapshot(), hosts: [{ ...host, defaultTui: "default" }], workspaces: [workspace], instances: [],
    });
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    expect(screen.getByTestId("new-session-tui")).toHaveValue("default");
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].tui).toBeUndefined();
  });

  it("does not show or submit a Claude renderer for other harnesses", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderWithCli();
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    fireEvent.change(screen.getByTestId("new-session-tui"), { target: { value: "default" } });
    fireEvent.click(screen.getByTestId("new-session-kind-codex"));
    expect(screen.queryByTestId("new-session-tui")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("new-session-start"));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].tui).toBeUndefined();
  });
});
