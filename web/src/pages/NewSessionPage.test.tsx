import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
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

it("mounts the composer's slider inline, not the old tier chips", () => {
  renderWithCli();
  const slider = screen.getByTestId("new-session-effort-slider");
  expect(slider).toHaveAttribute("role", "slider");
  expect(slider).toHaveAttribute("data-tiers", "default,think,think-hard,ultracode");
  expect(screen.getByTestId("new-session-effort-knob")).toBeInTheDocument();
  expect(screen.getByTestId("new-session-effort-track")).toBeInTheDocument();
  // The tier chips are gone; the hint that names the spec is not.
  expect(screen.queryByTestId("new-session-effort-think")).toBeNull();
  expect(screen.getByTestId("new-session-effort")).toHaveTextContent("写进 InstanceSpec，会话内可再改");
});

it("re-snaps the slider onto the new harness table when the runtime changes", () => {
  renderWithCli();
  const slider = () => screen.getByTestId("new-session-effort-slider");
  // Settings default: claude `think`, the second of four.
  expect(slider()).toHaveAttribute("data-name", "think");
  expect(slider()).toHaveAttribute("data-index", "1");

  // codex has the same four stops, so the position is kept and renamed.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-tiers", "low,medium,high,ultra");
  expect(slider()).toHaveAttribute("data-name", "medium");
  expect(slider()).toHaveAttribute("data-index", "1");

  // grok has three: 1/3 of the way along snaps onto `standard`.
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
  expect(slider()).toHaveAttribute("data-name", "ultracode");
  expect(slider()).toHaveAttribute("data-ember", "1");

  // grok's table is shorter; the ember tier must stay the ember tier.
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "max");
  expect(slider()).toHaveAttribute("data-index", "2");
  expect(slider()).toHaveAttribute("data-ember", "1");

  // ...and back, without the stale `ultracode` draft the unmounted card held.
  fireEvent.click(screen.getByTestId("new-session-kind-codex"));
  expect(slider()).toHaveAttribute("data-name", "ultra");
  expect(slider()).toHaveAttribute("data-ember", "1");
  fireEvent.keyDown(slider(), { key: "Home" });
  expect(slider()).toHaveAttribute("data-name", "low");
  fireEvent.click(screen.getByTestId("new-session-kind-grok"));
  expect(slider()).toHaveAttribute("data-name", "quick");
  expect(slider()).toHaveAttribute("data-ember", "0");
});

it("writes the slider's tier into the InstanceSpec it creates", async () => {
  const create = vi.spyOn(store.hubStore, "create").mockResolvedValue(mockDb.instances[0]);
  renderWithCli();
  const slider = screen.getByTestId("new-session-effort-slider");
  fireEvent.keyDown(slider, { key: "ArrowRight" });
  expect(slider).toHaveAttribute("data-name", "think-hard");
  fireEvent.click(screen.getByTestId("new-session-start"));
  await waitFor(() => expect(create).toHaveBeenCalledWith(expect.objectContaining({
    kind: "claude", effortIndex: 2, effortName: "think-hard",
  })));
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
