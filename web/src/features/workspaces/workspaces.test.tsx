import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { hubStore } from "../../lib/store";
import { known } from "../../types/wire";
import type { Workspace } from "../../types/workspace";
import { WorkspaceRegistration } from "./WorkspaceRegistration";
import { WorkspaceList } from "./WorkspaceList";
import { workspaceCwd } from "./path";

const workspace: Workspace = {
  id: "wsp_app", hostId: "hst_node", rootPath: "/home/dev/projects/app", label: "app",
  revision: "1", createdAt: "", updatedAt: "", writePolicy: "default", canonicalRoot: known("/home/dev/projects/app"),
};

afterEach(() => vi.restoreAllMocks());

it("joins optional subpaths and refuses other absolute roots or escaping parents", () => {
  expect(workspaceCwd(workspace.rootPath, "")).toBe(workspace.rootPath);
  expect(workspaceCwd(workspace.rootPath, "src/../tests")).toBe(`${workspace.rootPath}/tests`);
  expect(workspaceCwd("/", "")).toBe("/");
  for (const subpath of ["~", "$HOME/app", "/tmp/app", "../other", "src/../../other", "C:\\app"]) {
    expect(workspaceCwd(workspace.rootPath, subpath)).toBeNull();
  }
});

it("registers on the selected host and selects the Node's canonical workspace", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  const select = vi.fn();
  render(<WorkspaceRegistration hostId={workspace.hostId} onRegistered={select} />);
  fireEvent.click(screen.getByTestId("workspace-add"));
  fireEvent.change(screen.getByTestId("workspace-register-path"), { target: { value: "/home/dev/link/" } });
  fireEvent.click(screen.getByTestId("workspace-register-submit"));
  await waitFor(() => expect(select).toHaveBeenCalledWith(workspace));
  expect(register).toHaveBeenCalledWith(workspace.hostId, "/home/dev/link/");
  expect(screen.queryByTestId("workspace-register-path")).not.toBeInTheDocument();
});

it("rejects relative registration input and displays Node validation reasons", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockRejectedValue(new Error("outside workspace_roots: /home/dev/projects"));
  render(<WorkspaceRegistration hostId={workspace.hostId} onRegistered={vi.fn()} />);
  fireEvent.click(screen.getByTestId("workspace-add"));
  fireEvent.change(screen.getByTestId("workspace-register-path"), { target: { value: "~/app" } });
  fireEvent.click(screen.getByTestId("workspace-register-submit"));
  expect(await screen.findByRole("alert")).toHaveTextContent("绝对路径");
  expect(register).not.toHaveBeenCalled();
  fireEvent.change(screen.getByTestId("workspace-register-path"), { target: { value: "/tmp/app" } });
  fireEvent.click(screen.getByTestId("workspace-register-submit"));
  expect(await screen.findByRole("alert")).toHaveTextContent("outside workspace_roots: /home/dev/projects");
  expect(screen.getByTestId("workspace-register-path")).toHaveValue("/tmp/app");
});

it("removes the host's registration and keeps Node rejection visible", async () => {
  const unregister = vi.spyOn(hubStore, "unregisterWorkspace").mockRejectedValue(new Error("workspace is busy"));
  render(<WorkspaceList hostId={workspace.hostId} online workspaces={[workspace]} />);
  fireEvent.click(screen.getByRole("button", { name: `移除目录 ${workspace.rootPath}` }));
  expect(await screen.findByRole("alert")).toHaveTextContent("workspace is busy");
  expect(unregister).toHaveBeenCalledWith(workspace.hostId, workspace.rootPath);
  expect(screen.getByTestId("host-workspace")).toHaveTextContent(workspace.rootPath);
});
