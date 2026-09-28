import { expect, test, devices, type Browser, type BrowserContext, type Page } from "@playwright/test";
import { login } from "./hub-auth";

/**
 * c-homeend: after a fake-Node RESTART the /m home row at 390px speaks the
 * shared endReason() sentence instead of the raw wire code in failure red.
 *
 * The test builds its OWN iPhone 13 context, so the same assertions run on
 * the chromium engine under playwright.hub.config.ts and on the WebKit engine
 * (the owner's real iPhone combination) under playwright.webkit-hub.config.ts.
 *
 * The scenario mirrors m-ghostbadge: a `GHOSTNODE_RESTART` instance.send
 * drops the fake Node and reconnects under a new epoch whose inventory omits
 * the victim (but keeps a survivor — the Hub ignores a hello carrying an
 * empty inventory, ws.rs epoch guard), so reconcile settles the victim
 * `exited` / `node-epoch-changed`. That code is an INTERRUPTION, not a
 * failure: the home row must show 「Node 重启，会话已中断 · 可恢复」 in the
 * quiet muted colour, raw code only in the tooltip, with 恢复 still on the
 * row's one line at 390.
 */

test.describe.configure({ mode: "serial" });

const created: string[] = [];

function uniqueToken(): string {
  return `homeend-${Math.random().toString(36).slice(2, 8)}-${Date.now().toString(36).slice(-4)}`;
}

async function createSession(page: Page, prompt: string): Promise<string> {
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
  const workspacePicker = page.getByTestId("new-session-workspace");
  await expect(workspacePicker.locator("option")).not.toHaveCount(0, { timeout: 20_000 });
  await workspacePicker.selectOption("wsp_e2e");
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, { timeout: 20_000 });
  const id = new URL(page.url()).pathname.split("/").pop()!;
  created.push(id);
  return id;
}

/** GHOSTNODE_RESTART via instance.send: the fake Node restarts, dropping
 *  only this instance from the new-epoch inventory. */
async function restartNodeDropping(page: Page, id: string, origin: string): Promise<void> {
  const result = await page.request.post(`/v1/instances/${id}/commands`, {
    headers: { Origin: origin },
    data: {
      operation: "instance.send",
      payload: { prompt: "GHOSTNODE_RESTART" },
    },
  });
  expect(result.ok(), `instance.send: ${result.status()} ${await result.text()}`).toBe(true);
}

test("390 iPhone: node-restarted home row shows the human interruption label in a neutral colour, raw code only in the tooltip, and 恢复 on the row's first line", async ({
  browser,
}) => {
  test.slow();
  // Own phone context: under the hub config the default project is Desktop
  // Chrome (whose /m redirects to /sessions); creating an iPhone 13 context
  // makes the same 390px assertions run on the chromium engine AND, under
  // playwright.webkit-hub.config.ts, the WebKit engine the owner's iPhone
  // uses (same pattern as uo3-evidence).
  const context: BrowserContext = await browser.newContext({ ...devices["iPhone 13"] });
  const page: Page = await context.newPage();
  const victimToken = uniqueToken();
  const survivorToken = uniqueToken();
  try {
    await login(page);

    // The Hub refuses to reconcile against an EMPTY new-epoch inventory, so a
    // survivor the restart keeps announcing is a required part of the shape.
    const victimId = await createSession(page, `homeend-victim ${victimToken}`);
    await createSession(page, `homeend-survivor ${survivorToken}`);

    // Park the home away from /m while the Node dies: the list's 2.5s
    // screen/summary traffic shares the Node websocket. The restart itself is
    // an instance.send RPC (no xterm typing), so the mobile context can drive;
    // capture the app origin first because about:blank's is "null".
    const origin = new URL(page.url()).origin;
    await page.goto("about:blank");
    await restartNodeDropping(page, victimId, origin);
    await expect
      .poll(
        async () => {
          const row = (await (await page.request.get(`/v1/instances/${victimId}`)).json()) as {
            lifecycle?: string;
            lastError?: string;
          };
          return `${row.lifecycle ?? ""}|${row.lastError ?? ""}`;
        },
        { timeout: 30_000 },
      )
      .toBe("exited|node-epoch-changed");

    await page.goto("/m");
    await expect(page.getByTestId("home-list")).toBeVisible();
    await page.getByTestId("home-search").fill(victimToken);
    await expect(page.getByTestId("home-row")).toHaveCount(1, { timeout: 15_000 });
    const row = page.getByTestId("home-row");
    await expect(row).toHaveAttribute("data-status", "exited");

    // Human label; the raw machine code is nowhere in visible text.
    const body = row.getByTestId("home-row-body");
    await expect(body).toContainText("Node 重启，会话已中断");
    expect(await body.innerText(), "raw code never visible").not.toContain("node-epoch-changed");
    expect(await row.innerText(), "raw code nowhere on the row").not.toContain(
      "node-epoch-changed",
    );
    // The legacy error flag stays off: the tone carries the styling.
    await expect(body).toHaveAttribute("data-error", "0");
    await expect(body).toHaveAttribute("data-tone", "interrupted");
    // Tooltip: human sentence first line, raw code the second (desktop
    // SessionList/session banner use the same two-line shape).
    const title = await body.getAttribute("title");
    expect(title).toMatch(/^Node 重启，会话已中断[^\n]*\nnode-epoch-changed$/);

    // Neutral colour — not the failure red. Compare against the resolved role
    // tokens rather than a hard-coded rgb.
    const colors = await body.evaluate((el) => {
      const resolve = (prop: string, value: string) => {
        const probe = document.createElement("div");
        probe.style.setProperty(prop, value);
        document.body.appendChild(probe);
        const resolved = getComputedStyle(probe).getPropertyValue(prop);
        probe.remove();
        return resolved;
      };
      return {
        color: getComputedStyle(el).color,
        muted: resolve("color", "var(--fg-muted)"),
        danger: resolve("color", "var(--danger-fg)"),
      };
    });
    expect(colors.danger).not.toBe("");
    expect(colors.color, "interruption uses the quiet muted colour").toBe(colors.muted);
    expect(colors.color, "never the danger colour").not.toBe(colors.danger);

    // 恢复 stays on the row's ONE line at 390: its box begins beside (not
    // below) the reason text and never overlaps it horizontally, it stays in
    // the viewport, and the reason is a single truncated ellipsis line rather
    // than a second wrapped line.
    const resume = row.getByTestId("home-resume");
    await expect(resume).toBeVisible();
    const geom = await row.evaluate((rowEl) => {
      const bodyEl = rowEl.querySelector<HTMLElement>('[data-testid="home-row-body"]')!;
      const resumeEl = rowEl.querySelector<HTMLElement>('[data-testid="home-resume"]')!;
      const rb = bodyEl.getBoundingClientRect();
      const sb = resumeEl.getBoundingClientRect();
      const rowRect = rowEl.getBoundingClientRect();
      const bodyStyle = getComputedStyle(bodyEl);
      const lineHeight = parseFloat(bodyStyle.lineHeight);
      return {
        viewport: window.innerWidth,
        rowHeight: rowRect.height,
        // Wrapped layout puts the button on a SECOND flex line: its top lands
        // below the body bottom and its left edge retreats to the gutter.
        buttonBesideText: sb.top < rb.bottom - 0.5,
        buttonRightOfText: sb.left >= rb.right - 0.5,
        buttonRightInViewport: sb.right <= window.innerWidth + 0.5,
        bodyOneLine: rb.height <= lineHeight + 2,
        bodyNowrap: bodyStyle.whiteSpace === "nowrap",
        bodyEllipsis: bodyStyle.textOverflow.includes("ellipsis"),
        bodyClips: bodyStyle.overflow === "hidden",
      };
    });
    expect(geom.viewport, "iPhone descriptor is 390px wide").toBe(390);
    expect(geom.buttonBesideText, "恢复 never wraps below the reason").toBe(true);
    expect(geom.buttonRightOfText, "恢复 keeps its first-line slot to the right").toBe(true);
    expect(geom.buttonRightInViewport).toBe(true);
    expect(geom.bodyOneLine, "reason stays one line").toBe(true);
    expect(geom.bodyNowrap).toBe(true);
    expect(geom.bodyEllipsis).toBe(true);
    expect(geom.bodyClips).toBe(true);
    // Two text lines (title+body) plus padding only — a wrapped button would
    // add its own ~44px band on top.
    expect(geom.rowHeight, "row keeps its two-text-line height").toBeLessThan(80);

    // The survivor the restart kept announcing is still a live row.
    await page.getByTestId("home-search").fill(survivorToken);
    await expect(page.getByTestId("home-row")).toHaveCount(1, { timeout: 10_000 });
    await expect(page.getByTestId("home-row")).toHaveAttribute("data-status", /working|idle/);
  } finally {
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
    await context.close();
  }
});
