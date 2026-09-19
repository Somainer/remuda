import { expect, test, type Page } from "@playwright/test";
import pathFn from "node:path";
import { fileURLToPath } from "node:url";
import { login } from "./hub-auth";

/**
 * c-toolfold (D-041, ui-spec.md §2.2): on a compact (390px) layout settled
 * ordinary tool cards fold to one line carrying family + key argument
 * (Bash = command first line); Workflow cards never auto-fold and their live
 * clock keeps ticking; a card seen running folds the instant it settles;
 * expanding a fold mounts the full, desktop-identical card; the desktop
 * default stays unfolded. The explicit 全部折叠 keeps main's exact behaviour:
 * every non-failed card (running/Workflow included) folds. Driven by the fake
 * Node's `workflow card live` and `toolfold settle*` scenarios. No real
 * models.
 *
 * The failed-tool exemption under the automatic compact fold is covered at
 * unit level (toolRegistry.test.tsx): the shared fake harness has no
 * failed/denied ordinary-tool scenario, and interaction.* frames project to
 * QuestionForm rather than a ToolCard, so neither can mount a foldable card
 * here.
 */

const evidenceDir =
  process.env.REMUDA_EVIDENCE === "1"
    ? pathFn.resolve(pathFn.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence")
    : pathFn.resolve("test-results/evidence");

async function shot(page: Page, name: string): Promise<void> {
  await page.screenshot({ path: pathFn.join(evidenceDir, name) });
}

// Release every instance this spec creates (force is a u8 query param).
test.afterEach(async ({ page }) => {
  const m = page.url().match(/\/s\/([^/?#]+)/);
  if (!m) return;
  const res = await page.request.delete(`/v1/instances/${m[1]}?force=1`);
  expect(res.ok() || res.status() === 404).toBeTruthy();
});

async function raiseCap(page: Page, to: number): Promise<void> {
  const hosts = await page.evaluate(async () => {
    const response = await fetch("/v1/hosts", { credentials: "include" });
    return response.json() as Promise<{
      items?: { hostId?: string; id?: string; label?: string; maxInstances?: number }[];
    }>;
  });
  const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
  if (!host) return;
  const hostId = (host.hostId ?? host.id) as string;
  if ((host.maxInstances ?? 8) >= to) return;
  await page.evaluate(
    async ({ id, value }) => {
      await fetch(`/v1/hosts/${id}`, {
        method: "PATCH",
        credentials: "include",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ maxInstances: value }),
      });
    },
    { id: hostId, value: to },
  );
}

let hubRan = false;

test.beforeEach(async ({ page }) => {
  // Start at desktop width: on a 390px shell the 新建 link lives behind the
  // mobile nav and is not clickable; tests resize once the session is open.
  await page.setViewportSize({ width: 1280, height: 900 });
  await login(page);
  await raiseCap(page, 24);
  hubRan = true;
});

test.afterAll(async ({ browser }) => {
  if (!hubRan) return;
  const page = await browser.newPage();
  try {
    await login(page);
    const hosts = await page.evaluate(async () => {
      const response = await fetch("/v1/hosts", { credentials: "include" });
      return response.json() as Promise<{
        items?: { hostId?: string; id?: string; label?: string }[];
      }>;
    });
    const host = (hosts.items ?? []).find((h) => h.label === "e2e-fake-node");
    if (host) {
      await page.evaluate(
        async (id) => {
          await fetch(`/v1/hosts/${id}`, {
            method: "PATCH",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ maxInstances: 8 }),
          }).catch(() => {});
        },
        (host.hostId ?? host.id) as string,
      );
    }
  } finally {
    await page.close();
  }
});

async function answerPending(page: Page, instanceId: string): Promise<void> {
  await expect
    .poll(
      async () =>
        page.evaluate(async (id) => {
          const list = await fetch("/v1/interactions", { credentials: "include" });
          const body = (await list.json()) as {
            items?: {
              id: string;
              instanceId?: string;
              state?: string;
              request?: { kind?: string; inputDigest?: string; options?: { id: string }[] };
            }[];
          };
          const mine = (body.items ?? []).filter((item) => item.instanceId === id && item.state === "pending");
          for (const item of mine) {
            const optionId = item.request?.options?.[0]?.id;
            if (!optionId) continue;
            await fetch(`/v1/interactions/${item.id}/answer`, {
              method: "POST",
              credentials: "include",
              headers: { "content-type": "application/json" },
              body: JSON.stringify({
                answer: { kind: "approval", optionId, inputDigest: item.request?.inputDigest ?? "" },
              }),
            });
          }
          return mine.length;
        }, instanceId),
      { timeout: 20_000 },
    )
    .toBe(0);
}

async function openLiveSession(page: Page): Promise<string> {
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-prompt").fill("workflow card live");
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.getByTestId("composer-input").fill("workflow card live");
  await page.getByTestId("composer-send").click();
  // The undismissed live Workflow card stays outside the turn's compact fold;
  // the settled Bash + thought are summarised inside it.
  await expect(page.getByTestId("workflow-card").first()).toHaveAttribute("data-status", "running", {
    timeout: 20_000,
  });
  await expect(page.getByTestId("compact-fold")).toContainText("1 次工具 · 1 段思考");
  return instanceId;
}

/**
 * Start a scripted `toolfold settle*` session at desktop (creation needs the
 * desktop 新建 link), answer the launch approval, resize to 390px, and only
 * then send so every tool frame arrives in the compact layout.
 */
async function startSettleSessionAt390(page: Page, prompt: string): Promise<string> {
  await page.getByTitle("新建", { exact: true }).click();
  await expect(page.getByTestId("new-session-sheet")).toBeVisible();
  const host = await page
    .getByTestId("new-session-host")
    .locator("option")
    .filter({ hasText: "e2e-fake-node" })
    .getAttribute("value");
  await page.getByTestId("new-session-host").selectOption(host!);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await expect(page).toHaveURL(/\/s\//, { timeout: 20_000 });
  const instanceId = new URL(page.url()).pathname.split("/").pop() as string;
  await answerPending(page, instanceId);
  await expect(page.getByTestId("composer-input")).toBeEnabled({ timeout: 20_000 });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByTestId("composer-input").fill(prompt);
  await page.getByTestId("composer-send").click();
  return instanceId;
}

test("at 390px a settled Bash card folds to family + command first line; expanding mounts the full card", async ({
  page,
}) => {
  await openLiveSession(page);
  await page.setViewportSize({ width: 390, height: 844 });
  // Reload so the journal snapshot mounts the already-finished Bash call as
  // settled history; the Workflow run itself is still live and replays its
  // running state.
  await page.reload();
  await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("workflow-card").first()).toHaveAttribute("data-status", "running");

  // The ordinary Bash tool sits inside the turn's compact fold; open it to
  // reach the card. It starts folded to its one-line row (D-041).
  await page.getByTestId("compact-fold").click();
  const foldWrap = page.getByTestId("compact-fold-wrap");
  const bashCard = foldWrap.getByTestId("tool-card");
  await expect(bashCard).toHaveAttribute("data-folded", "1");
  const arg = bashCard.getByTestId("tool-fold-arg");
  await expect(arg).toHaveText("echo workflow-running");
  // The full command is reachable via the title attribute.
  expect(await arg.getAttribute("title")).toBe("echo workflow-running");
  // The family word (Bash) is the row heading; a bare duplicate family chip
  // would make the row read "Bash Bash".
  expect((await bashCard.textContent()) ?? "").toContain("Bash");
  // The full card body is not mounted while folded.
  await expect(foldWrap.getByText("$ echo workflow-running")).toHaveCount(0);
  await shot(page, "ux2026-toolfold-fold-390.png");

  // Expanding reveals the same card the desktop layout renders.
  await bashCard.getByTestId("tool-fold-open").click();
  await expect(bashCard).toHaveAttribute("data-folded", "0");
  await expect(foldWrap.getByText("$ echo workflow-running")).toBeVisible();
  await expect(foldWrap.getByText(/exit 0/)).toBeVisible();
  await shot(page, "ux2026-toolfold-expanded-390.png");
});

test.describe("with a coarse pointer", () => {
  // Folding follows the compact LAYOUT, not touch capability (ui-spec §3.4):
  // the same 390px fold happens on a touch device, and the Workflow
  // exemption with its 1s clock is pointer-independent.
  test.use({ hasTouch: true });

  test("at 390px the Workflow card never folds and its head elapsed keeps ticking", async ({ page }) => {
    await openLiveSession(page);
    await page.setViewportSize({ width: 390, height: 844 });
    // Snapshot mount: the Bash call is settled history; the Workflow run is
    // still live and replays its running state.
    await page.reload();
    await expect(page.getByTestId("session-page")).toBeVisible({ timeout: 20_000 });

    // The live Workflow timeline stays a top-level card, mounted (its 1Hz
    // interval therefore keeps running), wrapped as an unfolded tool card.
    const wfWrapper = page
      .locator('[data-testid="tool-card"]:has([data-testid="workflow-card"])')
      .first();
    await expect(wfWrapper).toHaveAttribute("data-folded", "0");
    const wfCard = page.getByTestId("workflow-card").first();
    await expect(wfCard).toHaveAttribute("data-status", "running");

    const head = page.getByTestId("workflow-card-head");
    const readClock = async (): Promise<string | null> => {
      const text = await head.textContent();
      return text?.match(/\d+m \d{2}s/)?.[0] ?? null;
    };
    const first = await readClock();
    expect(first, "head elapsed renders before asserting the tick").not.toBeNull();
    await shot(page, "ux2026-toolfold-wf-clock-a.png");
    // The hard D-041 acceptance metric: the head elapsed advances while
    // running — not merely "the card is visible".
    await expect.poll(readClock, { timeout: 5_000, intervals: [500] }).not.toBe(first);
    await shot(page, "ux2026-toolfold-wf-clock-b.png");

    // The settled Bash in the same turn still folds (layout decision), while
    // the Workflow card is not counted inside the compact fold.
    await page.getByTestId("compact-fold").click();
    await expect(page.getByTestId("compact-fold-wrap").getByTestId("tool-card")).toHaveAttribute(
      "data-folded",
      "1",
    );
    await expect(page.getByTestId("compact-fold-wrap").getByTestId("workflow-card")).toHaveCount(0);
  });
});

test("the desktop default stays unfolded; collapse-all folds every non-failed card (main behaviour)", async ({
  page,
}) => {
  await openLiveSession(page);

  // 1280px: the settled Bash card renders the full card by default.
  await page.getByTestId("compact-fold").click();
  const bashCard = page.getByTestId("compact-fold-wrap").getByTestId("tool-card");
  await expect(bashCard).toHaveAttribute("data-folded", "0");
  await expect(page.getByText("$ echo workflow-running")).toBeVisible();

  // Explicit collapse-all keeps main's exact behaviour: the ordinary card
  // folds AND the live Workflow card folds (its timeline inner unmounts) —
  // D-041's Workflow exemption governs only the automatic compact fold, not
  // this reader action.
  await page.getByTestId("collapse-all").click();
  await expect(bashCard).toHaveAttribute("data-folded", "1");
  await expect(page.getByTestId("workflow-card")).toHaveCount(0);
  const wfToggle = page.getByRole("button", { name: "展开 Workflow" });
  await expect(wfToggle).toHaveCount(1);

  // Expanding the Workflow row re-mounts the live card unchanged.
  await wfToggle.click();
  await expect(page.getByTestId("workflow-card").first()).toHaveAttribute("data-status", "running");
});

test("at 390px a tool seen running folds the instant it settles live — no reload", async ({ page }) => {
  await startSettleSessionAt390(page, "toolfold settle");
  const card = page.getByTestId("tool-card").first();

  // Frame 1: the call is running — still visible as the full card.
  await expect(card).toHaveAttribute("data-folded", "0");
  await expect(card).toContainText("echo toolfold-live-settle");
  await expect(card.getByTestId("tool-fold-open")).toHaveCount(0);

  // Frame 2 (3s later): the final result lands. The card folds on its own —
  // the primary D-041 path; no reload, no reader action.
  await expect(card).toHaveAttribute("data-folded", "1", { timeout: 10_000 });
  await expect(card.getByTestId("tool-fold-arg")).toHaveText("echo toolfold-live-settle");
  await shot(page, "ux2026-toolfold-live-settled-390.png");

  // Expanding still reaches the desktop-identical card.
  await card.getByTestId("tool-fold-open").click();
  await expect(card).toHaveAttribute("data-folded", "0");
  await expect(card).toContainText("exit 0");
});

test.describe("with a coarse pointer", () => {
  test.use({ hasTouch: true });

  test("a long MCP tool name truncates the folded row: no horizontal overflow, 44px fold target", async ({
    page,
  }) => {
    await startSettleSessionAt390(page, "toolfold settle mcp");
    const card = page.getByTestId("tool-card").first();
    await expect(card).toHaveAttribute("data-folded", "1", { timeout: 10_000 });
    const toggle = card.getByTestId("tool-fold-open");

    // The long qualified name stays inside the row (truncated, full name in
    // the accessible name) rather than pushing the row past 390px.
    await expect(toggle).toHaveAttribute(
      "aria-label",
      "展开 mcp__remuda-very-long-integration-server__search_files_everywhere",
    );

    // ui-spec §3.4: the only affordance has a var(--touch) (44px) hit zone via
    // the centred ::after, while the visual button stays its 22px box.
    const hotZone = await toggle.evaluate((el) => {
      const after = getComputedStyle(el, "::after");
      return { width: after.width, height: after.height };
    });
    expect(hotZone.width).toBe("44px");
    expect(hotZone.height).toBe("44px");

    // Computed pseudo geometry can lie about clipping: prove the hot zone's
    // top and bottom edges (centre ±21px) actually hit-test onto the toggle
    // or one of its descendants. .fold's overflow:hidden plus an 8px+22px+8px
    // head used to clip these pixels away.
    const edgeHits = await toggle.evaluate((el) => {
      const rect = el.getBoundingClientRect();
      const cx = rect.left + rect.width / 2;
      const cy = rect.top + rect.height / 2;
      const probe = (x: number, y: number) => {
        const hit = document.elementFromPoint(x, y);
        return Boolean(hit && (hit === el || el.contains(hit)));
      };
      return { top: probe(cx, cy - 21), bottom: probe(cx, cy + 21) };
    });
    expect(edgeHits.top).toBe(true);
    expect(edgeHits.bottom).toBe(true);

    // The button stays inside the viewport and the page has no horizontal
    // overflow even with the >40-char tool name.
    const box = await toggle.boundingBox();
    expect(box).toBeTruthy();
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(390);
    expect(box!.y).toBeGreaterThanOrEqual(0);
    const overflow = await page.evaluate(() => ({
      doc: document.documentElement.scrollWidth,
      win: window.innerWidth,
    }));
    expect(overflow.doc).toBeLessThanOrEqual(overflow.win + 1);
    await shot(page, "ux2026-toolfold-longname-390.png");
  });
});
