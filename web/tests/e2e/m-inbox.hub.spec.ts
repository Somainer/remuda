import { expect, test, type Page } from "@playwright/test";
import { mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-minbox / D-049 ui-spec §2.5 §4.7: the phone inbox at /m/inbox.
 *
 * 390px hub (fake Node) coverage:
 *  - the fake node raises one approval per fresh session; 允许一次 removes the
 *    row (journal receipt) and no second submit is possible;
 *  - an offline host pauses the row: every option button is disabled and the
 *    row carries 主机离线，交互暂停;
 *  - ?focus=<interactionId> scrolls that row into the viewport and marks it;
 *  - ?kind= filters with the same query semantics as /approvals, and question
 *    rows offer 去回答 to /s/:id instead of inline forms.
 */
test.describe.configure({ mode: "serial" });

const VIEWPORT_H = 844;

const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence
  ? path.join(here, "../../../docs/design/evidence")
  : path.join(here, "../../test-results/evidence");

const created: string[] = [];

async function shot(page: Page, name: string) {
  await mkdir(shotDir, { recursive: true });
  await page.screenshot({ path: path.join(shotDir, name), animations: "disabled" });
}

async function createSession(page: Page, prompt: string, workspaceId?: string): Promise<string> {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", { timeout: 20_000 });
  const hostId = await hostPicker
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId!);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20_000,
  });
  if (workspaceId) await page.getByTestId("new-session-workspace").selectOption(workspaceId);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

async function pendingInteractionId(page: Page, instanceId: string): Promise<string> {
  const id = await page.evaluate(async (iid) => {
    const body = await (await fetch("/v1/interactions", { credentials: "include" })).json();
    const found = (body.items ?? []).find(
      (item: { instanceId?: string; state?: string; interactionId?: string; id?: string }) =>
        item.instanceId === iid && item.state === "pending",
    );
    return found?.interactionId ?? found?.id ?? null;
  }, instanceId);
  expect(id, "the fake node raises a pending approval for a fresh session").toBeTruthy();
  return id as string;
}

function inboxRow(page: Page, interactionId: string) {
  return page.locator(`[data-interaction-id="${interactionId}"]`);
}

/**
 * A `claude-pty` session created straight through the REST API: it reaches
 * `running` (so it belongs to 进行中 · 最近) and its tty accepts
 * TTYNODE_RESTART, the fake Node's own epoch-bump fixture.
 */
async function createRunningPty(page: Page): Promise<string> {
  const hosts = (await (await page.request.get("/v1/hosts")).json()) as {
    items: { hostId: string; label: string }[];
  };
  const host = hosts.items.find((item) => item.label === "e2e-fake-node");
  expect(host).toBeTruthy();
  const workspaces = (await (
    await page.request.get(`/v1/hosts/${host!.hostId}/workspaces`)
  ).json()) as { workspaces: { workspaceId: string; root: string }[] };
  const workspace = workspaces.workspaces[0];
  expect(workspace).toBeTruthy();
  const create = await page.request.post("/v1/instances", {
    headers: { Origin: new URL(page.url()).origin },
    data: {
      hostId: host!.hostId,
      workspaceId: workspace.workspaceId,
      cwd: workspace.root,
      kind: "claude",
      driver: "claude-pty",
      model: "e2e/auto",
      name: "e2e-m-inbox-restart",
    },
  });
  expect(create.ok(), await create.text()).toBe(true);
  const instanceId = ((await create.json()) as { instance: { instanceId: string } }).instance
    .instanceId;
  created.push(instanceId);
  await expect
    .poll(
      async () =>
        ((await (await page.request.get(`/v1/instances/${instanceId}`)).json()) as {
          lifecycle?: string;
        }).lifecycle,
      { timeout: 20_000 },
    )
    .toBe("running");
  return instanceId;
}

async function restartNodeFromTty(page: Page, instanceId: string): Promise<void> {
  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
    timeout: 30_000,
  });
  await page
    .locator(".xterm-helper-textarea")
    .first()
    .evaluate((el) => (el as HTMLTextAreaElement).focus());
  await page.keyboard.type("TTYNODE_RESTART");
  await page.keyboard.press("Enter");
}

async function expectSettledByRestart(page: Page, instanceId: string): Promise<void> {
  await expect
    .poll(
      async () =>
        ((await (await page.request.get(`/v1/instances/${instanceId}`)).json()) as {
          lifecycle?: string;
          lastError?: string;
        }).lifecycle,
      { timeout: 30_000 },
    )
    .toBe("exited");
  expect(
    ((await (await page.request.get(`/v1/instances/${instanceId}`)).json()) as {
      lastError?: string;
    }).lastError,
  ).toBe("node-epoch-changed");
}

test.describe("390px phone inbox", () => {
  test.use({
    viewport: { width: 390, height: VIEWPORT_H },
    hasTouch: true,
    isMobile: true,
  });

  test.beforeEach(async ({ page }) => {
    await login(page);
  });

  test.afterEach(async ({ page }) => {
    // A 2 s store poll can still be inside the offline test's route callback
    // at teardown; drop the interception before the next test mounts.
    await page.unrouteAll({ behavior: "ignoreErrors" }).catch(() => undefined);
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });

  test("允许一次 removes the row and a second submit is impossible", async ({ page }) => {
    const instanceId = await createSession(page, "m inbox allow once");
    const interactionId = await pendingInteractionId(page, instanceId);

    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    const row = inboxRow(page, interactionId);
    await expect(row).toContainText("echo e2e");
    const allow = row.getByRole("button", { name: "允许一次" });
    await expect(allow).toBeVisible();
    await expect(row.getByRole("button", { name: "拒绝" })).toBeVisible();
    await expect(row.getByRole("link", { name: "打开会话" })).toBeVisible();

    await allow.click();
    // The option buttons leave the row immediately (answering -> 已提交, no
    // local lock grab); the row itself disappears when the journal receipt
    // lands. There is no control left that could submit a second answer.
    await expect(row).toHaveCount(0, { timeout: 20_000 });
    await expect(page.getByRole("button", { name: "允许一次" })).toHaveCount(0);
    await expect(page.getByTestId("m-inbox-pending-count")).toHaveText("待处理 0");

    // The answered instance turns idle and moves to 进行中 · 最近 — never a
    // third tier.
    const recent = page.locator(`[data-instance-id="${instanceId}"][data-testid="m-inbox-recent-row"]`);
    await expect(recent).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("m-inbox-tier-recent")).toContainText("进行中 · 最近");
    await expect(page.getByText("已离队")).toHaveCount(0);
  });

  test("an offline host disables the buttons and shows 主机离线，交互暂停", async ({ page }) => {
    const instanceId = await createSession(page, "m inbox paused host");
    const interactionId = await pendingInteractionId(page, instanceId);

    // The shared fake Node keeps serving other specs; emulate this host going
    // offline at the REST boundary (same approach as ux-files.hub.spec.ts).
    await page.route("**/v1/hosts", async (route) => {
      const response = await route.fetch();
      const body = await response.json();
      const patched = {
        ...body,
        items: (body.items ?? []).map((item: Record<string, unknown>) =>
          item.state === "online" || item.state === "enrolled"
            ? { ...item, state: "offline" }
            : item,
        ),
      };
      await route.fulfill({
        status: response.status(),
        contentType: response.headers()["content-type"] ?? "application/json",
        body: JSON.stringify(patched),
      });
    });

    await page.goto("/m/inbox");
    const row = inboxRow(page, interactionId);
    await expect(row.getByTestId("m-inbox-paused")).toHaveText("主机离线，交互暂停", {
      timeout: 15_000,
    });
    await expect(row.getByRole("button", { name: "允许一次" })).toBeDisabled();
    await expect(row.getByRole("button", { name: "拒绝" })).toBeDisabled();
    // The open-session link is not a submit path and stays reachable.
    await expect(row.getByRole("link", { name: "打开会话" })).toBeVisible();
  });

  test("?focus= highlights the row and scrolls it inside the viewport", async ({ page }) => {
    // Four pending rows: the oldest interaction sorts to the bottom of 待你处理
    // and starts well below the 844px fold.
    const pairs: Array<{ instance: string; interaction: string }> = [];
    for (let i = 0; i < 4; i += 1) {
      const instance = await createSession(page, `m inbox focus ${i}`);
      pairs.push({ instance, interaction: await pendingInteractionId(page, instance) });
    }
    const target = pairs[0].interaction;

    await page.goto(`/m/inbox?focus=${target}`);
    await expect(page.getByTestId("approval-row")).toHaveCount(4, { timeout: 20_000 });
    const focused = page.locator('[data-interaction-id="' + target + '"]');
    await expect(focused).toHaveAttribute("data-focus", "true");

    // The row's border box sits inside the 390x844 viewport after the deep
    // link scroll (block:center leaves clear margins on both sides).
    await expect
      .poll(
        async () => {
          const box = await focused.boundingBox();
          if (!box) return null;
          return { y: box.y, bottom: box.y + box.height };
        },
        { timeout: 10_000 },
      )
      .toMatchObject({
        y: expect.any(Number),
        bottom: expect.any(Number),
      });
    const box = await focused.boundingBox();
    expect(box, "focused row rendered").toBeTruthy();
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(VIEWPORT_H);
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(390);

    // No focus query -> no row is marked.
    await page.goto("/m/inbox");
    await expect(page.locator("[data-focus='true']")).toHaveCount(0);
  });

  test("?kind= filters like /approvals and question rows go 去回答 to /s/:id", async ({ page }) => {
    const approvalInstance = await createSession(page, "m inbox kind approval");
    await pendingInteractionId(page, approvalInstance);
    const questionInstance = await createSession(page, "ask-question m inbox kind question");
    const questionId = await pendingInteractionId(page, questionInstance);

    await page.goto("/m/inbox?kind=question");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("approval-row")).toHaveCount(1, { timeout: 20_000 });
    const qrow = inboxRow(page, questionId);
    await expect(qrow).toHaveAttribute("data-kind", "question");
    const answer = qrow.getByRole("link", { name: "去回答" });
    await expect(answer).toHaveAttribute("href", `/s/${questionInstance}`);
    await expect(qrow.getByRole("button", { name: "允许一次" })).toHaveCount(0);

    await page.goto("/m/inbox?kind=approval");
    await expect(page.getByTestId("approval-row").filter({ hasText: "echo e2e" })).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.locator(`[data-interaction-id="${questionId}"]`)).toHaveCount(0);

    // Segment buttons drive the same query; 全部 removes it.
    await page.getByTestId("inbox-kind-question").click();
    await expect(page).toHaveURL(/kind=question$/);
    await page.getByTestId("inbox-kind-all").click();
    await expect(page).toHaveURL(/\/m\/inbox$/);
    await expect(page.getByTestId("approval-row")).toHaveCount(2);
  });

  test("kind radiogroup is fully 44px tappable at the coarse band edges", async ({ page }) => {
    // UO-9 round-2: the shared segItem is 26px VISIBLE and its 44px reach is
    // the ::after band 9px above/below it. Inside an overflow-x scrollport
    // that band must stay hittable (kept inside the track's block padding,
    // not an outer margin the scrollport clips). Tap 2px inside the top and
    // bottom of the 44px band — both points are OUTSIDE the 26px item.
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    const group = page.getByRole("radiogroup", { name: "交互类型" });
    await expect(group).toBeVisible();
    const band = await group.boundingBox();
    expect(band, "kind track rendered").toBeTruthy();
    expect(band!.height).toBeGreaterThanOrEqual(43);

    const testIdAt = async (x: number, y: number): Promise<string | null> =>
      page.evaluate(
        ({ x, y }) => {
          const el = document.elementFromPoint(x, y) as HTMLElement | null;
          return el ? (el.dataset.testid ?? el.tagName) : null;
        },
        { x, y },
      );

    const approval = page.getByTestId("inbox-kind-approval");
    const ab = await approval.boundingBox();
    expect(ab).toBeTruthy();
    const cx = ab!.x + ab!.width / 2;
    const topY = band!.y + 1 + 2; // 2px inside the band top (above the item)
    const bottomY = band!.y + band!.height - 1 - 2; // 2px inside the band bottom
    // The ::after reach, not the 26px glyph box, owns these points.
    expect(await testIdAt(cx, topY)).toBe("inbox-kind-approval");
    expect(await testIdAt(cx, bottomY)).toBe("inbox-kind-approval");

    // Functional: a tap on the upper edge selects 审批; on the lower edge of
    // 全部 it clears again.
    await page.mouse.click(cx, topY);
    await expect(page).toHaveURL(/kind=approval$/);
    const all = page.getByTestId("inbox-kind-all");
    const allBox = await all.boundingBox();
    expect(allBox).toBeTruthy();
    await page.mouse.click(allBox!.x + allBox!.width / 2, bottomY);
    await expect(page).toHaveURL(/\/m\/inbox$/);
  });

  test("a Node-restarted session leaves 进行中 for a quiet neutral 最近结束 row", async ({
    page,
    browser,
  }) => {
    test.slow();
    // The mobile page is logged in by beforeEach and lands on /m, whose home
    // list polls screens/summaries for every instance every few seconds. That
    // traffic queues on the shared fake Node's single websocket and can starve
    // the TTYNODE_RESTART bytes, so park it on a blank page: create, restart
    // and verify the settle entirely from a desktop page (the deterministic
    // node-inventory ordering), and mount the inbox only once the row is
    // settled.
    await page.goto("about:blank");
    // browser.newPage() inherits THIS test's mobile context (isMobile/touch),
    // under which the xterm helper swallows raw keyboard input and the
    // sentinel never reaches the node. The restart is driven from a separate,
    // explicitly-desktop context (cookies are copied by login), mirroring
    // node-inventory.hub.spec's non-touch run.
    const desktopContext = await browser.newContext({
      viewport: { width: 1280, height: 800 },
      hasTouch: false,
      isMobile: false,
    });
    const desktop = await desktopContext.newPage();
    await login(desktop, "m-inbox-restart-driver");
    // The Hub ignores an epoch hello that carries NO instance inventory (the
    // 2026-09-18 demo guard: never wipe rows on a silent node). The fixture
    // drops only the session whose tty sent the sentinel, so create a
    // surviving pty too — the exact shape node-inventory.hub.spec uses.
    const instanceId = await createRunningPty(desktop);
    const survivorId = await createRunningPty(desktop);
    await restartNodeFromTty(desktop, instanceId);
    await expectSettledByRestart(desktop, instanceId);
    void survivorId; // both ids are in created[] and force-deleted by afterEach
    await desktopContext.close().catch(() => undefined);

    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    // Never under a 进行中 heading.
    await expect(
      page.locator(`[data-testid="m-inbox-recent-row"][data-instance-id="${instanceId}"]`),
    ).toHaveCount(0);

    // It lands in the quiet, collapsed 最近结束 group.
    const ended = page.getByTestId("m-inbox-ended");
    await expect(ended).toBeVisible();
    const endedRow = ended
      .locator(`[data-testid="m-inbox-ended-row"][data-instance-id="${instanceId}"]`)
      .first();
    await expect(endedRow).toHaveCount(1);
    await expect(endedRow).toBeHidden();

    // Expand: the human sentence, the neutral tone, and the raw code nowhere
    // in visible text — only in the tooltip.
    await page.getByTestId("m-inbox-ended-summary").click();
    await expect(endedRow).toBeVisible();
    await expect(endedRow).toHaveAttribute("data-end-tone", "interrupted");
    const sentence = endedRow.getByText("Node 重启，会话已中断");
    await expect(sentence).toBeVisible();
    expect(endedRow).not.toContainText("node-epoch-changed");
    await expect(sentence).toHaveAttribute("title", "node-epoch-changed");

    // Neutral colour: interrupted is muted, never the danger red. Compare
    // resolved rgb() values (the token is a hex/var, not an rgb literal).
    const colors = await sentence.evaluate((el) => {
      const probe = document.createElement("span");
      probe.style.color = "var(--danger-fg)";
      document.body.appendChild(probe);
      const danger = getComputedStyle(probe).color;
      probe.remove();
      return { actual: getComputedStyle(el).color, danger };
    });
    expect(colors.danger).not.toBe("");
    expect(colors.actual).not.toBe(colors.danger);

    // The row opens the session page, which keeps the Resume affordance.
    if (evidence) {
      await shot(page, "mobile-ui-5-inbox-ended-390.png");
      const evidenceContext = await browser.newContext({
        viewport: { width: 1440, height: 900 },
        hasTouch: false,
        isMobile: false,
      });
      const desktop = await evidenceContext.newPage();
      await login(desktop, "m-inbox-ended-evidence");
      await desktop.goto(`/s/${instanceId}`);
      await expect(desktop.getByTestId("node-restart-banner")).toContainText(
        "Node 重启，会话已中断",
        { timeout: 20_000 },
      );
      await shot(desktop, "desktop-endreason-node-restart-1440.png");
      await evidenceContext.close();
    }
  });

  test("evidence: inbox banner and both tiers at 390px", async ({ page }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshot");
    await page.emulateMedia({ reducedMotion: "reduce" });
    // One answered session -> idle row in 进行中 · 最近; one still pending.
    const answeredInstance = await createSession(page, "m inbox evidence recent");
    const answeredInteraction = await pendingInteractionId(page, answeredInstance);
    await page.goto("/m/inbox");
    const answeredRow = inboxRow(page, answeredInteraction);
    await answeredRow.getByRole("button", { name: "允许一次" }).click();
    await expect(answeredRow).toHaveCount(0, { timeout: 20_000 });
    await createSession(page, "m inbox evidence pending");

    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox-push-banner")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("approval-row")).toHaveCount(1, { timeout: 15_000 });
    await expect(page.getByTestId("m-inbox-recent-row")).toHaveCount(1, { timeout: 15_000 });
    await shot(page, "mobile-ui-4-inbox-390.png");
  });
});
