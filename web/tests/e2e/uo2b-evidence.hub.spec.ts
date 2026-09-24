import { expect, test, type Page } from "@playwright/test";
import { writeFile, mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setMode } from "./appearanceHelper";
import { login } from "./hub-auth";

/**
 * UO-2b evidence: the Spaces index panel, QuickFind, the /s/* tab strip in
 * both containers (a bound Task's sessions across Spaces vs the Space
 * fallback), the strip's overflow edge cues, and the compact chips/drawer —
 * Remuda's own renders at 390 and 1440 in both modes.
 *
 * Compact /s/:id renders no strip (D-049): the phone strip lives on /m and
 * the session page folds switching into the header chip + drawer.
 *
 * A default run skips everything; REMUDA_EVIDENCE=1 writes
 * docs/design/evidence/ui-overhaul/UO-2b-<surface>-<mode>-<width>.png.
 */

test.skip(!process.env.REMUDA_EVIDENCE, "set REMUDA_EVIDENCE=1 to capture the committed screenshots");
test.describe.configure({ mode: "serial" });

const shotDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence/ui-overhaul");
const MODES = ["dark", "light"] as const;
const WIDTHS = [390, 1440] as const;
type Mode = (typeof MODES)[number];

// Wire id grammar: tsk_ prefix + canonical lowercase UUIDv7 (scalar.rs); the
// task need not exist in the ledger — get_task None just skips worktree binding.
const TASK_ID = "tsk_01900000-0000-7000-8000-0000000002b0";
const created: string[] = [];

async function hostId(page: Page): Promise<string> {
  const hosts = (await (await page.request.get("/v1/hosts", { credentials: "include" })).json()) as {
    items?: { hostId?: string; label?: string }[];
  };
  const id = hosts.items?.find((host) => host.label === "e2e-fake-node")?.hostId ?? hosts.items?.[0]?.hostId;
  expect(id, "an e2e fake node must be registered").toBeTruthy();
  return id!;
}

async function patchMaxInstances(page: Page, value: number): Promise<void> {
  await page.evaluate(async (next) => {
    const list = await fetch("/v1/hosts", { credentials: "include" });
    const body = (await list.json()) as { items?: { hostId?: string }[] };
    const id = body.items?.[0]?.hostId;
    if (id) await fetch(`/v1/hosts/${id}`, {
      method: "PATCH", credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ maxInstances: next }),
    });
  }, value);
}

async function createSession(page: Page, host: string, workspaceId: string, taskId?: string): Promise<string> {
  const response = await page.request.post("/v1/instances", {
    headers: { "content-type": "application/json" },
    data: {
      hostId: host,
      workspaceId,
      kind: "claude",
      driver: "claude-print",
      prompt: taskId ? "UO-2b evidence task session" : "UO-2b evidence space session",
      ...(taskId ? { taskId } : {}),
    },
  });
  expect(response.ok(), `create ${response.status()} ${await response.text()}`).toBe(true);
  const instanceId = ((await response.json()) as { instance: { instanceId: string } }).instance.instanceId;
  created.push(instanceId);
  return instanceId;
}

async function shoot(page: Page, surface: string, mode: Mode, width: number): Promise<void> {
  await setMode(page, mode);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(200);
  await mkdir(shotDir, { recursive: true });
  await writeFile(
    path.join(shotDir, `UO-2b-${surface}-${mode}-${width}.png`),
    await page.screenshot({ animations: "disabled" }),
  );
}

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    await patchMaxInstances(page, 16);
    const host = await hostId(page);
    // One unbound session (Space fallback) and one task bound across both
    // registered workspaces (D-053 Task container). Six tabs share the
    // primary Space on /m and nine task tabs overflow the 1440px strip.
    await createSession(page, host, "wsp_e2e");
    for (let i = 0; i < 9; i += 1) {
      await createSession(page, host, i % 2 === 0 ? "wsp_e2e" : "wsp_e2e_second", TASK_ID);
    }
  } finally {
    await page.close();
  }
});

test.afterAll(async ({ browser }) => {
  const page = await browser.newPage();
  try {
    await login(page);
    await patchMaxInstances(page, 8);
    for (const id of created) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  } finally {
    await page.close();
  }
});

test.beforeEach(async ({ page }) => {
  await login(page);
});

for (const width of WIDTHS) {
  test(`UO-2b surfaces at ${width}`, async ({ page }) => {
    test.setTimeout(120_000);
    const phone = width < 768;
    await page.setViewportSize({ width, height: phone ? 844 : 900 });
    await page.emulateMedia({ reducedMotion: "reduce" });

    const strip = page.getByTestId("space-tabs");

    // Resolve the nine task tabs in createdAt order; the middle one sits
    // between off-screen tabs in both directions on desktop.
    const createdBody = await page.request
      .get("/v1/instances", { failOnStatusCode: true })
      .then((response) => response.json()) as { items?: { instanceId: string; taskId?: string | null; createdAt?: string }[] };
    const taskIds = (createdBody.items ?? [])
      .filter((row) => created.includes(row.instanceId) && row.taskId === TASK_ID)
      .sort((a, b) => (a.createdAt ?? "").localeCompare(b.createdAt ?? "") || a.instanceId.localeCompare(b.instanceId))
      .map((row) => row.instanceId);
    expect(taskIds.length).toBe(9);
    const middleTask = taskIds[5];

    if (phone) {
      // A fresh context selects the alphabetically first Space; pin the
      // primary Space (six open tabs) through a deep link, which records the
      // Space selection, then return to the compact home.
      await page.goto(`/s/${created[0]}`);
      await expect(page.getByTestId("spaces-chips")).toBeVisible();
      // UO-3: the compact home carries no chips row and no strip — the header
      // Space button opens the same drawer. The task/space tab surface only
      // exists on the /s/* session route (still reachable via the deep link
      // above).
      await page.goto("/m");
      await expect(page.getByTestId("space-chip")).toHaveCount(0);
      await expect(strip).toHaveCount(0);
      await expect(page.getByTestId("spaces-drawer-open")).toBeVisible();
      for (const mode of MODES) await shoot(page, "home", mode, width);

      // The same panel as the desktop index lives behind the drawer, and the
      // drawer hosts QuickFind.
      await page.getByTestId("spaces-drawer-open").click();
      await expect(page.getByTestId("spaces-drawer")).toBeVisible();
      for (const mode of MODES) await shoot(page, "drawer", mode, width);
      await page.getByTestId("quickfind-trigger").click();
      await expect(page.getByTestId("quickfind-panel")).toBeVisible();
      await page.getByTestId("quickfind-input").fill("e2e");
      for (const mode of MODES) await shoot(page, "quickfind", mode, width);
      await page.keyboard.press("Escape");
    } else {
      // Desktop: the Spaces index column on /sessions, with QuickFind.
      await page.goto("/sessions");
      await expect(page.getByTestId("spaces-panel")).toBeVisible();
      for (const mode of MODES) await shoot(page, "index", mode, width);

      await page.getByTestId("quickfind-trigger").click();
      await expect(page.getByTestId("quickfind-panel")).toBeVisible();
      await page.getByTestId("quickfind-input").fill("e2e");
      for (const mode of MODES) await shoot(page, "quickfind", mode, width);
      await page.keyboard.press("Escape");

      // Task container: nine tabs across two Spaces. scrollIntoView only
      // guarantees the active tab stays visible, not that it lands mid-strip;
      // for the edge-cue evidence frame, center the strip so both cues show.
      await page.goto(`/s/${middleTask}`);
      await expect(strip).toBeVisible();
      await strip.evaluate((element) => {
        element.scrollLeft = (element.scrollWidth - element.clientWidth) / 2;
        element.dispatchEvent(new Event("scroll"));
      });
      await expect(strip).toHaveAttribute("data-overflow", "both");
      await expect(strip).toHaveAccessibleName("本任务的会话");
      await expect(strip.getByRole("tab", { selected: true })).toHaveCount(1);
      for (const mode of MODES) await shoot(page, "task-tabs", mode, width);

      // Unbound session: the strip falls back to Space tabs.
      await page.goto(`/s/${created[0]}`);
      await expect(strip).toBeVisible();
      await expect(strip).toHaveAccessibleName(/^空间 /);
      for (const mode of MODES) await shoot(page, "space-tabs", mode, width);
    }
  });
}
