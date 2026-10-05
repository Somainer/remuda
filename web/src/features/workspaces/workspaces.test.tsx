import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { api } from "../../lib/api";
import { hubStore } from "../../lib/store";
import { known } from "../../types/wire";
import type { Workspace } from "../../types/workspace";
import { DirBrowser } from "./DirBrowser";
import { WorkspaceRegistration } from "./WorkspaceRegistration";
import { WorkspaceList } from "./WorkspaceList";
import { workspaceCwd } from "./path";
import type { HostDirsListing } from "./dirs";

const workspace: Workspace = {
  id: "wsp_app", hostId: "hst_node", rootPath: "/home/dev/projects/app", label: "app",
  revision: "1", createdAt: "", updatedAt: "", writePolicy: "default", canonicalRoot: known("/home/dev/projects/app"),
};

const listing = (over: Partial<HostDirsListing> = {}): HostDirsListing => ({
  path: "/home/dev",
  parent: null,
  home: "/home/dev",
  roots: ["/home/dev"],
  workspaces: [],
  dirs: [{ name: "projects" }, { name: "tools" }, { name: ".config" }],
  truncated: false,
  ...over,
});

beforeEach(() => {
  vi.spyOn(api, "hostDirsList").mockImplementation((_hostId, query) =>
    Promise.resolve(listing({
      path: query?.path ?? "/home/dev",
      dirs: listing().dirs.filter((entry) => query?.showHidden || !entry.name.startsWith(".")),
    })),
  );
});

afterEach(() => vi.restoreAllMocks());

it("joins optional subpaths and refuses other absolute roots or escaping parents", () => {
  expect(workspaceCwd(workspace.rootPath, "")).toBe(workspace.rootPath);
  expect(workspaceCwd(workspace.rootPath, "src/../tests")).toBe(`${workspace.rootPath}/tests`);
  expect(workspaceCwd("/", "")).toBe("/");
  for (const subpath of ["~", "$HOME/app", "/tmp/app", "../other", "src/../../other", "C:\\app"]) {
    expect(workspaceCwd(workspace.rootPath, subpath)).toBeNull();
  }
});

it("opens the browser and registers the Node's canonical workspace for the current folder", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  const select = vi.fn();
  render(<WorkspaceRegistration hostId={workspace.hostId} onRegistered={select} />);
  fireEvent.click(screen.getByTestId("workspace-add"));
  const browser = await screen.findByTestId("dir-browser");
  expect(await within(browser).findAllByTestId("dir-browser-row")).toHaveLength(2);
  expect(within(browser).queryByText(".config")).toBeNull();

  fireEvent.click(within(browser).getByTestId("dir-browser-use"));
  await waitFor(() => expect(select).toHaveBeenCalledWith(workspace));
  expect(register).toHaveBeenCalledWith(workspace.hostId, "/home/dev");
});

it("navigates into a folder, filters, and refreshes with hidden folders shown", async () => {
  const dirsList = vi.mocked(api.hostDirsList);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  const browser = await screen.findByTestId("dir-browser");

  // Descend into projects.
  fireEvent.click(within(browser).getByText("projects"));
  await waitFor(() =>
    expect(dirsList).toHaveBeenLastCalledWith(workspace.hostId, { path: "/home/dev/projects", showHidden: false }),
  );

  // Filter box narrows the current folder's rows (client-side only).
  fireEvent.change(within(browser).getByTestId("dir-browser-filter"), { target: { value: "too" } });
  expect(within(browser).queryByText("projects")).toBeNull();
  expect(within(browser).getByText("tools")).toBeTruthy();

  // Toggling hidden folders re-requests the same folder with showHidden.
  fireEvent.click(within(browser).getByTestId("dir-browser-hidden"));
  await waitFor(() =>
    expect(dirsList.mock.calls.at(-1)).toEqual([
      workspace.hostId,
      { path: "/home/dev/projects", showHidden: true },
    ]),
  );
});

it("shows Node listing errors and keeps the dialog open", async () => {
  vi.mocked(api.hostDirsList).mockRejectedValue(new Error("host offline"));
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  expect(await screen.findByRole("alert")).toHaveTextContent("host offline");
  expect(screen.queryAllByTestId("dir-browser-row")).toHaveLength(0);
});

it("keeps manual absolute-path entry as an advanced option", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  await screen.findByTestId("dir-browser");
  fireEvent.click(screen.getByTestId("dir-browser-manual-toggle"));
  fireEvent.change(screen.getByTestId("dir-browser-manual-path"), { target: { value: "/srv/app" } });
  fireEvent.click(screen.getByTestId("dir-browser-manual-submit"));
  await waitFor(() => expect(register).toHaveBeenCalledWith(workspace.hostId, "/srv/app"));
});

it("rejects a relative manual path before calling the API", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  await screen.findByTestId("dir-browser");
  fireEvent.click(screen.getByTestId("dir-browser-manual-toggle"));
  fireEvent.change(screen.getByTestId("dir-browser-manual-path"), { target: { value: "projects/app" } });
  fireEvent.click(screen.getByTestId("dir-browser-manual-submit"));
  expect(await screen.findByRole("alert")).toHaveTextContent("绝对路径");
  expect(register).not.toHaveBeenCalled();
});

it("removes the host's registration only after confirmation and shows Node refusals", async () => {
  vi.spyOn(window, "confirm").mockReturnValue(true);
  const unregister = vi.spyOn(hubStore, "unregisterWorkspace").mockRejectedValue(new Error("still used by 1 live session(s)"));
  render(<WorkspaceList hostId={workspace.hostId} online workspaces={[workspace]} />);
  fireEvent.click(screen.getByRole("button", { name: `移除目录 ${workspace.rootPath}` }));
  expect(window.confirm).toHaveBeenCalledOnce();
  expect(await screen.findByRole("alert")).toHaveTextContent("still used by 1 live session(s)");
  expect(unregister).toHaveBeenCalledWith(workspace.hostId, workspace.rootPath);
  expect(screen.getByTestId("host-workspace")).toHaveTextContent(workspace.rootPath);
});

it("does not remove when the confirmation is cancelled", async () => {
  vi.spyOn(window, "confirm").mockReturnValue(false);
  const unregister = vi.spyOn(hubStore, "unregisterWorkspace").mockResolvedValue();
  render(<WorkspaceList hostId={workspace.hostId} online workspaces={[workspace]} />);
  fireEvent.click(screen.getByRole("button", { name: `移除目录 ${workspace.rootPath}` }));
  expect(unregister).not.toHaveBeenCalled();
});
