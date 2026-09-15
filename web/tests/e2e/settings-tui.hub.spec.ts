import { expect, test, type Page, type Route } from "@playwright/test";
import { login, logout } from "./hub-auth";

type TuiMode = "fullscreen" | "default";
type FakeHost = { hostId?: string; id?: string; label?: string; defaultTui?: TuiMode | null };

async function readDefault(page: Page, hostPath: string): Promise<TuiMode | null> {
  return page.evaluate(async (path) => {
    const response = await fetch(path, { credentials: "include" });
    if (!response.ok) throw new Error(`host read failed: ${response.status}`);
    const host = await response.json() as { defaultTui?: "fullscreen" | "default" | null };
    return host.defaultTui ?? null;
  }, hostPath);
}

// This spec changes only the synthetic host's launch preference. It never
// creates an instance, invokes a real Node, or contacts a model provider.
test("Settings saves a host renderer explicitly and rolls back a rejected change", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Requires the isolated Hub fake-node fixture");
  const rejection = "synthetic host-default rejection";
  const patches: unknown[] = [];
  let hostPath: string | undefined;
  let priorDefault: TuiMode | null = null;
  let restoreDefault = false;
  let loggedIn = false;
  let rejectNext = false;
  let releasePatch: (() => void) | undefined;
  let activeRoute: Promise<void> = Promise.resolve();
  let routePattern: string | undefined;

  const interceptPatch = (route: Route): Promise<void> => {
    if (route.request().method() !== "PATCH") return route.continue();
    patches.push(route.request().postDataJSON());
    const reject = rejectNext;
    activeRoute = (async () => {
      // Hold the response so saving is observable without a timing race.
      await new Promise<void>((resolve) => { releasePatch = resolve; });
      if (reject) {
        await route.fulfill({ status: 409, json: { error: rejection } });
      } else {
        const response = await route.fetch();
        await route.fulfill({ response });
      }
    })();
    return activeRoute;
  };

  try {
    await login(page, "settings-tui-e2e-browser");
    loggedIn = true;
    const hosts = await page.evaluate(async () => {
      const response = await fetch("/v1/hosts", { credentials: "include" });
      if (!response.ok) throw new Error(`host list failed: ${response.status}`);
      return response.json() as Promise<{ items?: FakeHost[] }>;
    });
    const fakeHosts = (hosts.items ?? []).filter((host) => host.label === "e2e-fake-node");
    expect(fakeHosts, "only the synthetic fixture host may be patched").toHaveLength(1);
    const hostId = fakeHosts[0]!.hostId ?? fakeHosts[0]!.id;
    expect(hostId).toBeTruthy();
    hostPath = `/v1/hosts/${hostId}`;
    priorDefault = await readDefault(page, hostPath);
    restoreDefault = true;
    const initial = priorDefault ?? "fullscreen";
    const selected: TuiMode = initial === "fullscreen" ? "default" : "fullscreen";
    routePattern = `**${hostPath}`;
    await page.route(routePattern, interceptPatch);

    await page.goto("/settings#appearance");
    await page.getByTestId("settings-nav-host-defaults").click();
    await expect(page).toHaveURL(/\/settings#host-defaults$/);
    await expect(page.getByTestId("settings-group-host-defaults")).toBeInViewport();
    const fieldId = `settings-host-defaults-${hostId}`;
    const select = page.getByTestId(`settings-host-tui-${hostId}`);
    const save = page.getByTestId(`${fieldId}-save`);
    const status = page.getByTestId(`${fieldId}-status`);
    await expect(select).toHaveValue(initial);
    await expect(save).toBeDisabled();

    await select.selectOption(selected);
    await expect(save).toBeEnabled();
    expect(patches).toEqual([]);
    expect(await readDefault(page, hostPath)).toBe(priorDefault);
    await save.click();
    await expect.poll(() => patches.length).toBe(1);
    expect(patches[0]).toEqual({ defaultTui: selected });
    await expect(status).toHaveAttribute("data-phase", "saving");
    await expect(status).toContainText("保存中");
    await expect(select).toBeDisabled();
    releasePatch!();
    await expect(status).toHaveAttribute("data-phase", "saved");
    await expect(status).toContainText("已保存");
    expect(await readDefault(page, hostPath)).toBe(selected);

    // A fresh page must load the confirmed value from the Hub.
    await page.reload();
    await expect(page.getByTestId("settings-group-host-defaults")).toBeInViewport();
    await expect(select).toHaveValue(selected);
    rejectNext = true;
    await select.selectOption(initial);
    await expect(save).toBeEnabled();
    expect(patches).toHaveLength(1);
    await save.click();
    await expect.poll(() => patches.length).toBe(2);
    expect(patches[1]).toEqual({ defaultTui: initial });
    await expect(status).toHaveAttribute("data-phase", "saving");
    releasePatch!();
    await expect(status).toHaveAttribute("data-phase", "error");
    await expect(status).toContainText("失败");
    await expect(status).toContainText(rejection);
    await expect(select).toHaveValue(selected);
    await expect(save).toBeDisabled();
    expect(await readDefault(page, hostPath)).toBe(selected);
  } finally {
    // Drain a held route before restoring so a delayed successful write
    // cannot overwrite cleanup. Restore null as null, preserving inheritance.
    releasePatch?.();
    await activeRoute.catch(() => undefined);
    if (routePattern) await page.unroute(routePattern, interceptPatch);
    try {
      if (restoreDefault && hostPath) {
        const restored = await page.evaluate(async ({ path, defaultTui }) => {
          const response = await fetch(path, {
            method: "PATCH",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ defaultTui }),
          });
          return response.status;
        }, { path: hostPath, defaultTui: priorDefault });
        expect(restored, "restore the shared fake host's prior default").toBe(200);
        expect(await readDefault(page, hostPath)).toBe(priorDefault);
      }
    } finally {
      if (loggedIn) {
        await page.goto("/settings#connection");
        await logout(page);
      }
    }
  }
});
