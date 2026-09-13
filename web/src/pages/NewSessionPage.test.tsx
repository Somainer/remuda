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
