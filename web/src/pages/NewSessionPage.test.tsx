import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import * as store from "../lib/store";
import { mockDb } from "../lib/mock";
import { NewSessionPage } from "./NewSessionPage";

vi.mock("../lib/viewport", () => ({
  useWorkbenchViewport: () => ({ mobile: false }), composing: () => false,
}));

const host = { ...mockDb.hosts[0], state: "online" as const };
const workspace = { ...mockDb.workspaces[0], id: "wsp_project", hostId: host.id, rootPath: "/home/dev/projects/app" };

beforeEach(() => {
  localStorage.clear();
  vi.spyOn(api, "providerList").mockResolvedValue({ items: [] });
  vi.spyOn(store, "useHub").mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [host], workspaces: [workspace], instances: [],
  });
});
afterEach(() => vi.restoreAllMocks());

it("creates with the registered ID and a cwd inside the chosen workspace", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
  expect(screen.getByTestId("new-session-cwd")).toHaveValue("");
  fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "src" } });
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    hostId: host.id, workspaceId: workspace.id, cwd: "/home/dev/projects/app/src",
  })));
});

it("blocks absolute or escaping cwd text instead of sending it to another root", () => {
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
  fireEvent.change(screen.getByTestId("new-session-cwd"), { target: { value: "~" } });
  expect(screen.getByRole("alert")).toHaveTextContent("相对子路径");
  expect(screen.getByTestId("new-session-start")).toBeDisabled();
});

it("creates a worktree in the selected project and uses its returned path", async () => {
  const createTree = vi.spyOn(store.hubStore, "createWorktree").mockResolvedValue({ name: "feature", path: "/home/dev/projects/app-wt/feature" });
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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

function renderWithCli() {
  vi.mocked(store.useHub).mockReturnValue({
    ...store.hubStore.getSnapshot(), hosts: [cliHost], workspaces: [workspace], instances: [],
  });
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
  render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
  expect(screen.getByTestId("new-session-effort-foot")).toHaveTextContent(
    "写进 InstanceSpec，会话内可再改",
  );
  expect(screen.getByTestId("new-session-effort")).toHaveTextContent("写进 InstanceSpec，会话内可再改");
  // The standalone ultracode chip is gone; ultracode is the last tick.
  expect(screen.queryByTestId("new-session-effort-ultracode")).toBeNull();
});

it("re-snaps the slider onto the new harness table when the runtime changes", () => {
  renderWithCli();
  const slider = () => screen.getByTestId("new-session-effort-slider");
  // Settings default: claude `high` at the midpoint.
  expect(slider()).toHaveAttribute("data-name", "high");
  expect(slider()).toHaveAttribute("data-index", "2");

  // codex: the midpoint (2/4) maps onto `high` by nearest position.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-tiers", "low,medium,high,ultra");
  expect(slider()).toHaveAttribute("data-name", "high");
  expect(slider()).toHaveAttribute("data-index", "2");
  // ultracode is Claude-only; other harnesses never show the extra stop.
  expect(slider()).toHaveAttribute("aria-valuemax", "3");

  // grok has three: the midpoint snaps onto `standard`.
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-tiers", "quick,standard,max");
  expect(slider()).toHaveAttribute("data-name", "standard");
  expect(slider()).toHaveAttribute("data-index", "1");
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

  // grok's table is shorter; the flag drops and the ember tier maps ember→ember.
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "max");
  expect(slider()).toHaveAttribute("data-index", "2");
  expect(slider()).toHaveAttribute("data-ember", "1");

  // ...and back, without the stale draft the unmounted card held.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-name", "ultra");
  expect(slider()).toHaveAttribute("data-ember", "1");
  fireEvent.keyDown(slider(), { key: "Home" });
  expect(slider()).toHaveAttribute("data-name", "low");
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "quick");
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

it("writes the slider's tier into the InstanceSpec it creates", async () => {
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
    kind: "grok", effortIndex: 2, effortName: "max",
  })));
});

describe("D-028 native PTY default", () => {
  it("offers 原生终端 shell-pty as the default for claude and creates with it", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    renderWithLaunchableMatrix();
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
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
    fireEvent.click(screen.getByTestId("new-session-kind-grok"));
    expect(screen.getByTestId("new-session-driver-generic-pty")).toHaveAttribute("data-default", "1");
    expect(screen.getByTestId("new-session-driver-shell-pty")).toBeDisabled();
  });
});

describe("特殊参数 and claude 可执行文件", () => {
  it("splits args on whitespace into an argv array rather than sending a shell string", async () => {
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
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
    render(<MemoryRouter><NewSessionPage /></MemoryRouter>);
    fireEvent.click(screen.getByTestId("new-session-advanced"));
    // Typing over a prefilled default would turn an operator's host-level
    // choice into a session value, and the Hub merges the two differently.
    expect(screen.getByTestId("new-session-args")).toHaveValue("");
    expect(screen.getByTestId("new-session-args")).toHaveAttribute("placeholder", "--effort max");
    expect(screen.getByTestId("new-session-binary")).toHaveValue("");
    expect(screen.getByTestId("new-session-binary")).toHaveAttribute("placeholder", "/srv/claude");
  });
});
