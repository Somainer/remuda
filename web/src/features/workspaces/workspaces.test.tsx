import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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

it("registers a Node listing path that really ends in a space verbatim", async () => {
  // Round 5 item 3: hostDirsList returns a canonical path with a real trailing
  // space (a legal filename byte); use-this-folder must register exactly
  // that string, with no trim.
  const spaced = "/srv/remuda-e2e ";
  vi.mocked(api.hostDirsList).mockResolvedValue(
    listing({ path: spaced, home: spaced, roots: [spaced], dirs: [] }),
  );
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  await screen.findByTestId("dir-browser");
  fireEvent.click(screen.getByTestId("dir-browser-use"));
  await waitFor(() =>
    expect(register).toHaveBeenCalledWith(workspace.hostId, spaced),
  );
});

it("sends a filesystem-selected path verbatim and a typed path verbatim (no trim)", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  await screen.findByTestId("dir-browser");
  // The chosen folder path is sent verbatim.
  fireEvent.click(screen.getByTestId("dir-browser-use"));
  await waitFor(() =>
    expect(register).toHaveBeenCalledWith(workspace.hostId, "/home/dev"),
  );
  register.mockClear();
  // A typed path keeps its trailing space; leading whitespace is accepted
  // for the absolute check but the whole typed string is sent verbatim.
  fireEvent.click(screen.getByTestId("dir-browser-manual-toggle"));
  fireEvent.change(screen.getByTestId("dir-browser-manual-path"), {
    target: { value: "/srv/app " },
  });
  fireEvent.click(screen.getByTestId("dir-browser-manual-submit"));
  await waitFor(() =>
    expect(register).toHaveBeenCalledWith(workspace.hostId, "/srv/app "),
  );
});

it("Enter in the filter never submits a surrounding form; buttons still activate (portal)", async () => {
	// Round 4 items 6/7: the modal portals OUT of the surrounding form, and
	// Enter is intercepted only on text inputs. userEvent fires the real key
	// sequence (including implicit submit), unlike a bare keyDown.
	const user = userEvent.setup();
	const submitted = vi.fn();
	render(
		<form data-testid="outer-form" onSubmit={(event) => {
			event.preventDefault();
			submitted();
		}}>
			<button type="submit">start session</button>
			<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />
		</form>,
	);
	await screen.findByTestId("dir-browser");

	// Real Enter in the filter: no outer form submit.
	const filter = screen.getByTestId("dir-browser-filter");
	await user.click(filter);
	await user.keyboard("{Enter}");
	expect(submitted).not.toHaveBeenCalled();

	// Enter on a focused directory ROW activates it (native button
	// activation): it navigates (a browse for the child folder), and still
	// does not submit the outer form.
	const dirsList = vi.mocked(api.hostDirsList);
	const projects = screen.getByRole("button", { name: /projects/i });
	projects.focus();
	await user.keyboard("{Enter}");
	await waitFor(() =>
		expect(dirsList).toHaveBeenLastCalledWith(workspace.hostId, {
			path: "/home/dev/projects",
			showHidden: false,
		}),
	);
	expect(submitted).not.toHaveBeenCalled();

	// Enter on the Cancel button activates it natively (no form submit).
	const cancel = screen.getByTestId("dir-browser-cancel");
	cancel.focus();
	await user.keyboard("{Enter}");
	expect(submitted).not.toHaveBeenCalled();
});

it("an advanced-path Enter registers the exact path keeping a real trailing space", async () => {
	const user = userEvent.setup();
	const submitted = vi.fn();
	const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
	render(
		<form data-testid="outer-form" onSubmit={(event) => {
			event.preventDefault();
			submitted();
		}}>
			<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />
		</form>,
	);
	await screen.findByTestId("dir-browser");
	await user.click(screen.getByTestId("dir-browser-manual-toggle"));
	const input = screen.getByTestId("dir-browser-manual-path");
	await user.type(input, "/srv/app "); // round 4 item 10: real trailing space
	await user.keyboard("{Enter}");
	await waitFor(() =>
		expect(register).toHaveBeenCalledWith(workspace.hostId, "/srv/app "),
	);
	expect(submitted).not.toHaveBeenCalled();
});

it("the use-this-folder button keyboard-activates and registers the listing path", async () => {
	const user = userEvent.setup();
	const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
	render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
	await screen.findByTestId("dir-browser");
	const useButton = screen.getByTestId("dir-browser-use");
	(useButton as HTMLElement).focus();
	await user.keyboard("{Enter}");
	await waitFor(() =>
		expect(register).toHaveBeenCalledWith(workspace.hostId, "/home/dev"),
	);
});

it("Shift+Tab from the first modal control wraps to the last and never reaches the session form", async () => {
	// Round 5 item 5: the real nesting is Sheet(session form) -> portalled
	// browser modal. Shift+Tab on the first focusable modal control must wrap
	// to the LAST modal control, never to a control in the outer form.
	const user = userEvent.setup();
	const outerFirst = document.createElement("button");
	outerFirst.type = "button";
	outerFirst.dataset.testid = "outer-first";
	outerFirst.textContent = "outer first";
	document.body.appendChild(outerFirst);
	const submitted = vi.fn();
	try {
		render(
			<form onSubmit={(event) => {
				event.preventDefault();
				submitted();
			}}>
				<button type="button" data-testid="form-before">form before</button>
				<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />
			</form>,
		);
		await screen.findByTestId("dir-browser");
		// Focus the filter (first focusable control after the panel itself).
		const filter = screen.getByTestId("dir-browser-filter") as HTMLElement;
		filter.focus();
		expect(document.activeElement).toBe(filter);
		// Shift+Tab must wrap to the last modal control, not leave the modal.
		await user.tab({ shift: true });
		const active = document.activeElement;
		const modal = document.querySelector('[role="dialog"]');
		expect(modal, "portalled dialog exists").toBeTruthy();
		expect(modal!.contains(active), "focus stays inside the modal after Shift+Tab").toBe(true);
		expect((active as HTMLElement).dataset.testid ?? "", "wrapped to a modal control").not.toBe("form-before");
		expect(submitted).not.toHaveBeenCalled();
	} finally {
		outerFirst.remove();
	}
});

it("drops only leading whitespace from a typed path, keeping trailing whitespace", async () => {
	// Round 5 item 6: validity and the sent value derive from one string —
	// "  /srv/app  " enables the button, registers as "/srv/app  ".
	const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
	render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
	await screen.findByTestId("dir-browser");
	fireEvent.click(screen.getByTestId("dir-browser-manual-toggle"));
	const input = screen.getByTestId("dir-browser-manual-path");
	fireEvent.change(input, { target: { value: "   /srv/app  " } });
	expect((screen.getByTestId("dir-browser-manual-submit") as HTMLButtonElement).disabled).toBe(false);
	fireEvent.click(screen.getByTestId("dir-browser-manual-submit"));
	await waitFor(() =>
		expect(register).toHaveBeenCalledWith(workspace.hostId, "/srv/app  "),
	);
});

it("keeps the hidden-folder preference for the life of the modal across navigation", async () => {
  const dirsList = vi.mocked(api.hostDirsList);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  const browser = await screen.findByTestId("dir-browser");
  fireEvent.click(screen.getByTestId("dir-browser-hidden"));
  await waitFor(() =>
    expect(dirsList.mock.calls.at(-1)?.[1]).toMatchObject({ showHidden: true }),
  );
  await expect(within(browser).getByText(".config")).toBeVisible();
  fireEvent.click(within(browser).getByText("projects"));
  await waitFor(() =>
    expect(dirsList.mock.calls.at(-1)).toEqual([
      workspace.hostId,
      { path: "/home/dev/projects", showHidden: true },
    ]),
  );
});

it("rejects a relative manual path before calling the API", async () => {
  const register = vi.spyOn(hubStore, "registerWorkspace").mockResolvedValue(workspace);
  render(<DirBrowser hostId={workspace.hostId} open onClose={vi.fn()} onRegistered={vi.fn()} />);
  await screen.findByTestId("dir-browser");
  fireEvent.click(screen.getByTestId("dir-browser-manual-toggle"));
  const manualInput = screen.getByTestId("dir-browser-manual-path");
  fireEvent.change(manualInput, { target: { value: "projects/app" } });
  // The submit button is disabled for a non-absolute value; pressing Enter in
  // the field surfaces the validation error without calling the API.
  fireEvent.keyDown(manualInput, { key: "Enter" });
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
