import { expect, test, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";
const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), "__screenshots__");
/** 768/1024 layout checks are asserted but never committed: captures for
 *  those widths go to the worktree's ignored scratch dir under evidence only. */
const scratchDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../.e2e-log/evidence/gallery");
const COMMITTED_TAGS = new Set(["1440", "390"]);

// Goldens are committed artifacts: an ordinary gate run must navigate and
// assert every route but never write into the tracked tree. Captures happen
// only with REMUDA_EVIDENCE=1.
const capture = process.env.REMUDA_EVIDENCE === "1";

async function shot(page: Page, name: string, tag: string) {
  if (!capture) return;
  const targetDir = COMMITTED_TAGS.has(tag) ? dir : scratchDir;
  await mkdir(targetDir, { recursive: true });
  await writeFile(path.join(targetDir, name), await page.screenshot({ animations: "disabled", fullPage: false }));
}

test.describe("ui screenshots 1440 / 390", () => {
  test.describe.configure({ mode: "serial" });

  test("gallery of every primary route", async ({ page }, info) => {
    test.skip(info.project.name !== "chromium", "golden screenshots from chromium only");
    // Four widths sequentially, with a host-detail round-trip + flag clip
    // check at 390 and 768.
    test.setTimeout(75_000);
    await page.emulateMedia({ reducedMotion: "reduce" });

    // The mock client has no /v1/projects endpoint; give the gallery a small
    // directory (incl. a long name for the 390px header-overflow check) so
    // /projects renders its list instead of the load-error state.
    await page.route("**/v1/projects", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          items: [
            {
              id: "prj_sfe",
              name: "sfe-root",
              defaultBaseBranch: "main",
              branchPattern: "wt/{worker}/{topic}",
              members: [{ hostId: "hst_devbox", workspaceId: "wsp_sfe", role: "member" }],
            },
            {
              id: "prj_remuda",
              name: "remuda",
              defaultBaseBranch: "main",
              members: [
                { hostId: "hst_devbox", workspaceId: "wsp_app", role: "member" },
                { hostId: "hst_sg", workspaceId: "wsp_sg", role: "member" },
              ],
            },
            {
              id: "prj_long",
              name: "MMMMMMMMMMMMMMMMMMMMMMMMMM",
              defaultBaseBranch: "main",
              members: [{ hostId: "hst_devbox", workspaceId: "wsp_app", role: "member" }],
            },
          ],
        }),
      });
    });

    for (const [w, h, tag] of [
      [1440, 900, "1440"],
      [1024, 768, "1024"],
      [768, 1024, "768"],
      [390, 844, "390"],
    ] as const) {
      await page.setViewportSize({ width: w, height: h });

      // c-minbox: below 768 /sessions redirects to the phone home /m
      // (D-049); the session index column is desktop-only.
      if (w < 768) {
        await page.goto("/m");
        await expect(page.getByTestId("home-list")).toBeVisible();
        if (w >= 1024 || w < 768) await shot(page, `sessions-${tag}.png`, tag);
        await page.getByTestId("home-row-link").filter({ hasText: "看 TaskManager spill" }).first().click();
      } else {
        await page.goto("/sessions");
        await expect(page.getByTestId("session-list").first()).toBeVisible();
        if (w >= 1024 || w < 768) await shot(page, `sessions-${tag}.png`, tag);
        await page.getByTestId("session-row").filter({ hasText: "看 TaskManager spill" }).first().click();
      }
      await expect(page.getByTestId("session-page")).toBeVisible();
      await expect(page.getByTestId("composer")).toBeVisible();
      if (w >= 1024 || w < 768) await shot(page, `session-${tag}.png`, tag);

      await page.goto(`/s/${TTY_LAB}/tty`);
      await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
      await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
      if (w >= 1024 || w < 768) await shot(page, `tty-${tag}.png`, tag);

      await page.goto("/sessions/new");
      await expect(page.getByTestId("new-session-sheet")).toBeVisible();
      if (w >= 1024 || w < 768) await shot(page, `new-${tag}.png`, tag);

      await page.goto("/approvals");
      // c-minbox: below 768 the /approvals entry redirects to the phone inbox
      // /m/inbox (D-049); the desktop approvals centre is wide only.
      if (w < 768) {
        await expect(page).toHaveURL(/\/m\/inbox/);
        await expect(page.getByTestId("m-inbox")).toBeVisible();
      } else {
        await expect(page.getByTestId("approvals-page")).toBeVisible();
      }
      if (w >= 1024 || w < 768) await shot(page, `approvals-${tag}.png`, tag);

      await page.goto("/hosts");
      await expect(page.getByTestId("hosts-page")).toBeVisible();
      await shot(page, `hosts-${tag}.png`, tag);
      // The status line, CLI inventory and session count must be readable at
      // every width (the mobile summary carries them under 1024px).
      await expect(page.getByTestId("hosts-head").locator("h1")).toHaveText("主机");
      const hostSummary = page.getByTestId("host-row").first();
      await expect(hostSummary).toContainText("在线");
      await expect(hostSummary).toContainText(/claude|codex|grok/);
      await expect(hostSummary).toContainText(/会话\s*\d+/);
      // No horizontal overflow at any breakpoint.
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(w);

      // The host detail install flag must be present at 390 too.
      if (w === 390 || w === 768) {
        await page.getByTestId("host-row").first().click();
        await expect(page.getByTestId("host-detail")).toBeVisible();
        await expect(page.getByTestId("host-cli-flags").first()).toBeVisible();
        await expect(page.getByTestId("host-cli-flags").first()).toContainText(/已安装|未安装/);
        // At 768 the expanded sidebar narrows content: the full install flag
        // text must wrap inside the row, never clip (scrollWidth == clientWidth
        // and every glyph of the long string is painted).
        if (w === 768) {
          const flag = page.getByTestId("host-cli-flags").filter({ hasText: "nativeGateway" }).first();
          await expect(flag).toContainText("已安装");
          await expect(flag).toContainText("nativeGateway false");
          const noClip = await flag.evaluate((el) => el.scrollWidth <= el.clientWidth + 1);
          expect(noClip).toBe(true);
        }
        await page.goto("/hosts");
        await expect(page.getByTestId("hosts-page")).toBeVisible();
      }

      await page.goto("/fleet");
      await expect(page.getByTestId("fleet-page")).toBeVisible();
      await shot(page, `fleet-${tag}.png`, tag);

      await page.goto("/projects");
      await expect(page.getByTestId("projects-page")).toBeVisible();
      await shot(page, `projects-${tag}.png`, tag);
      // A long project name must not push the header past the viewport.
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(w);

      await page.goto("/providers");
      await expect(page.getByTestId("providers-page")).toBeVisible();
      await shot(page, `providers-${tag}.png`, tag);

      await page.goto("/bots");
      await expect(page.getByTestId("bots-page")).toBeVisible();
      await shot(page, `bots-${tag}.png`, tag);

      // The login card only renders unauthenticated. The mock otherwise
      // bootstraps every page into an authed session, so use a separate
      // browser context whose pre-boot storage is cleared and flagged
      // logged-out (the flag alone is not enough — a stored session auto
      // re-logs back in). A separate context keeps the main page's session.
      const loginBrowser = await page.context().browser();
      if (!loginBrowser) throw new Error("no browser for the login shot");
      const loginContext = await loginBrowser.newContext({
        viewport: { width: w, height: h },
        reducedMotion: "reduce",
      });
      const loginPage = await loginContext.newPage();
      await loginPage.addInitScript(() => {
        localStorage.clear();
        localStorage.setItem("runtime.logged-out", "1");
      });
      await loginPage.goto("/login");
      await expect(loginPage.getByTestId("login-page")).toBeVisible();
      await expect(loginPage.getByTestId("login-head")).toBeVisible();
      await shot(loginPage, `login-${tag}.png`, tag);
      await loginContext.close();

      await page.goto("/settings");
      await expect(page.getByTestId("settings-page")).toBeVisible();
      if (w >= 1024 || w < 768) await shot(page, `settings-${tag}.png`, tag);
    }

    await page.addInitScript(() => {
      sessionStorage.setItem("remuda.install-banner", "1");
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/sessions");
    await expect(page.getByTestId("install-bar")).toBeVisible();
    await shot(page, "install-390.png", "390"); // 390-only shot, unchanged
  });
});
