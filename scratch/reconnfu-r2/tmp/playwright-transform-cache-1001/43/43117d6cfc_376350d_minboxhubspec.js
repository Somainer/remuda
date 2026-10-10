// 954a41ee6790f13bf56c240cd271303a6d06ac2a
import { expect, test } from "@playwright/test";
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
test.describe.configure({
  mode: "serial"
});
const VIEWPORT_H = 844;
const here = path.dirname(fileURLToPath(import.meta.url));
const evidence = process.env.REMUDA_EVIDENCE === "1";
const shotDir = evidence ? path.join(here, "../../../docs/design/evidence") : path.join(here, "../../test-results/evidence");
const created = [];
async function shot(page, name) {
  await mkdir(shotDir, {
    recursive: true
  });
  await page.screenshot({
    path: path.join(shotDir, name),
    animations: "disabled"
  });
}
async function createSession(page, prompt, workspaceId) {
  await page.goto("/sessions/new");
  const hostPicker = page.getByTestId("new-session-host");
  await expect(hostPicker).toContainText("e2e-fake-node", {
    timeout: 20000
  });
  const hostId = await hostPicker.locator("option").filter({
    hasText: "e2e-fake-node"
  }).getAttribute("value");
  expect(hostId).toBeTruthy();
  await hostPicker.selectOption(hostId);
  await page.getByTestId("new-session-kind-claude").click();
  await expect(page.getByTestId("new-session-workspace").locator("option")).not.toHaveCount(0, {
    timeout: 20000
  });
  if (workspaceId) await page.getByTestId("new-session-workspace").selectOption(workspaceId);
  await page.getByTestId("new-session-prompt").fill(prompt);
  await page.getByTestId("new-session-start").click();
  await page.waitForURL(/\/s\//, {
    timeout: 20000
  });
  const id = new URL(page.url()).pathname.split("/").pop();
  created.push(id);
  return id;
}
async function pendingInteractionId(page, instanceId) {
  const id = await page.evaluate(async iid => {
    var _body$items, _ref, _found$interactionId;
    const body = await (await fetch("/v1/interactions", {
      credentials: "include"
    })).json();
    const found = ((_body$items = body.items) !== null && _body$items !== void 0 ? _body$items : []).find(item => item.instanceId === iid && item.state === "pending");
    return (_ref = (_found$interactionId = found === null || found === void 0 ? void 0 : found.interactionId) !== null && _found$interactionId !== void 0 ? _found$interactionId : found === null || found === void 0 ? void 0 : found.id) !== null && _ref !== void 0 ? _ref : null;
  }, instanceId);
  expect(id, "the fake node raises a pending approval for a fresh session").toBeTruthy();
  return id;
}
function inboxRow(page, interactionId) {
  return page.locator(`[data-interaction-id="${interactionId}"]`);
}

/**
 * A `claude-pty` session created straight through the REST API: it reaches
 * `running` (so it belongs to 进行中 · 最近) and its tty accepts
 * TTYNODE_RESTART, the fake Node's own epoch-bump fixture.
 */
async function createRunningPty(page) {
  const hosts = await (await page.request.get("/v1/hosts")).json();
  const host = hosts.items.find(item => item.label === "e2e-fake-node");
  expect(host).toBeTruthy();
  const workspaces = await (await page.request.get(`/v1/hosts/${host.hostId}/workspaces`)).json();
  const workspace = workspaces.workspaces[0];
  expect(workspace).toBeTruthy();
  const create = await page.request.post("/v1/instances", {
    headers: {
      Origin: new URL(page.url()).origin
    },
    data: {
      hostId: host.hostId,
      workspaceId: workspace.workspaceId,
      cwd: workspace.root,
      kind: "claude",
      driver: "claude-pty",
      model: "e2e/auto",
      name: "e2e-m-inbox-restart"
    }
  });
  expect(create.ok(), await create.text()).toBe(true);
  const instanceId = (await create.json()).instance.instanceId;
  created.push(instanceId);
  await expect.poll(async () => (await (await page.request.get(`/v1/instances/${instanceId}`)).json()).lifecycle, {
    timeout: 20000
  }).toBe("running");
  return instanceId;
}
async function restartNodeFromTty(page, instanceId) {
  await page.goto(`/s/${instanceId}/tty`);
  await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", {
    timeout: 30000
  });
  await page.locator(".xterm-helper-textarea").first().evaluate(el => el.focus());
  await page.keyboard.type("TTYNODE_RESTART");
  await page.keyboard.press("Enter");
}
async function expectSettledByRestart(page, instanceId) {
  await expect.poll(async () => (await (await page.request.get(`/v1/instances/${instanceId}`)).json()).lifecycle, {
    timeout: 30000
  }).toBe("exited");
  expect((await (await page.request.get(`/v1/instances/${instanceId}`)).json()).lastError).toBe("node-epoch-changed");
}
test.describe("390px phone inbox", () => {
  test.use({
    viewport: {
      width: 390,
      height: VIEWPORT_H
    },
    hasTouch: true,
    isMobile: true
  });
  test.beforeEach(async ({
    page
  }) => {
    await login(page);
  });
  test.afterEach(async ({
    page
  }) => {
    // A 2 s store poll can still be inside the offline test's route callback
    // at teardown; drop the interception before the next test mounts.
    await page.unrouteAll({
      behavior: "ignoreErrors"
    }).catch(() => undefined);
    for (const id of created.splice(0)) {
      await page.request.delete(`/v1/instances/${id}?force=1`).catch(() => undefined);
    }
  });
  test("允许一次 removes the row and a second submit is impossible", async ({
    page
  }) => {
    const instanceId = await createSession(page, "m inbox allow once");
    const interactionId = await pendingInteractionId(page, instanceId);
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    const row = inboxRow(page, interactionId);
    await expect(row).toContainText("echo e2e");
    const allow = row.getByRole("button", {
      name: "允许一次"
    });
    await expect(allow).toBeVisible();
    await expect(row.getByRole("button", {
      name: "拒绝"
    })).toBeVisible();
    await expect(row.getByRole("link", {
      name: "打开会话"
    })).toBeVisible();
    await allow.click();
    // The option buttons leave the row immediately (answering -> 已提交, no
    // local lock grab); the row itself disappears when the journal receipt
    // lands. There is no control left that could submit a second answer.
    await expect(row).toHaveCount(0, {
      timeout: 20000
    });
    await expect(page.getByRole("button", {
      name: "允许一次"
    })).toHaveCount(0);
    await expect(page.getByTestId("m-inbox-pending-count")).toHaveText("待处理 0");

    // The answered instance turns idle and moves to 进行中 · 最近 — never a
    // third tier.
    const recent = page.locator(`[data-instance-id="${instanceId}"][data-testid="m-inbox-recent-row"]`);
    await expect(recent).toBeVisible({
      timeout: 10000
    });
    await expect(page.getByTestId("m-inbox-tier-recent")).toContainText("进行中 · 最近");
    await expect(page.getByText("已离队")).toHaveCount(0);
  });
  test("an offline host disables the buttons and shows 主机离线，交互暂停", async ({
    page
  }) => {
    const instanceId = await createSession(page, "m inbox paused host");
    const interactionId = await pendingInteractionId(page, instanceId);

    // The shared fake Node keeps serving other specs; emulate this host going
    // offline at the REST boundary (same approach as ux-files.hub.spec.ts).
    await page.route("**/v1/hosts", async route => {
      var _body$items2, _response$headers$con;
      const response = await route.fetch();
      const body = await response.json();
      const patched = {
        ...body,
        items: ((_body$items2 = body.items) !== null && _body$items2 !== void 0 ? _body$items2 : []).map(item => item.state === "online" || item.state === "enrolled" ? {
          ...item,
          state: "offline"
        } : item)
      };
      await route.fulfill({
        status: response.status(),
        contentType: (_response$headers$con = response.headers()["content-type"]) !== null && _response$headers$con !== void 0 ? _response$headers$con : "application/json",
        body: JSON.stringify(patched)
      });
    });
    await page.goto("/m/inbox");
    const row = inboxRow(page, interactionId);
    await expect(row.getByTestId("m-inbox-paused")).toHaveText("主机离线，交互暂停", {
      timeout: 15000
    });
    await expect(row.getByRole("button", {
      name: "允许一次"
    })).toBeDisabled();
    await expect(row.getByRole("button", {
      name: "拒绝"
    })).toBeDisabled();
    // The open-session link is not a submit path and stays reachable.
    await expect(row.getByRole("link", {
      name: "打开会话"
    })).toBeVisible();
  });
  test("?focus= highlights the row and scrolls it inside the viewport", async ({
    page
  }) => {
    // Four pending rows: the oldest interaction sorts to the bottom of 待你处理
    // and starts well below the 844px fold.
    const pairs = [];
    for (let i = 0; i < 4; i += 1) {
      const instance = await createSession(page, `m inbox focus ${i}`);
      pairs.push({
        instance,
        interaction: await pendingInteractionId(page, instance)
      });
    }
    const target = pairs[0].interaction;
    await page.goto(`/m/inbox?focus=${target}`);
    await expect(page.getByTestId("approval-row")).toHaveCount(4, {
      timeout: 20000
    });
    const focused = page.locator('[data-interaction-id="' + target + '"]');
    await expect(focused).toHaveAttribute("data-focus", "true");

    // The row's border box sits inside the 390x844 viewport after the deep
    // link scroll (block:center leaves clear margins on both sides).
    await expect.poll(async () => {
      const box = await focused.boundingBox();
      if (!box) return null;
      return {
        y: box.y,
        bottom: box.y + box.height
      };
    }, {
      timeout: 10000
    }).toMatchObject({
      y: expect.any(Number),
      bottom: expect.any(Number)
    });
    const box = await focused.boundingBox();
    expect(box, "focused row rendered").toBeTruthy();
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.y + box.height).toBeLessThanOrEqual(VIEWPORT_H);
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(390);

    // No focus query -> no row is marked.
    await page.goto("/m/inbox");
    await expect(page.locator("[data-focus='true']")).toHaveCount(0);
  });
  test("?kind= filters like /approvals and question rows go 去回答 to /s/:id", async ({
    page
  }) => {
    const approvalInstance = await createSession(page, "m inbox kind approval");
    await pendingInteractionId(page, approvalInstance);
    const questionInstance = await createSession(page, "ask-question m inbox kind question");
    const questionId = await pendingInteractionId(page, questionInstance);
    await page.goto("/m/inbox?kind=question");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.getByTestId("approval-row")).toHaveCount(1, {
      timeout: 20000
    });
    const qrow = inboxRow(page, questionId);
    await expect(qrow).toHaveAttribute("data-kind", "question");
    const answer = qrow.getByRole("link", {
      name: "去回答"
    });
    await expect(answer).toHaveAttribute("href", `/s/${questionInstance}`);
    await expect(qrow.getByRole("button", {
      name: "允许一次"
    })).toHaveCount(0);
    await page.goto("/m/inbox?kind=approval");
    await expect(page.getByTestId("approval-row").filter({
      hasText: "echo e2e"
    })).toBeVisible({
      timeout: 10000
    });
    await expect(page.locator(`[data-interaction-id="${questionId}"]`)).toHaveCount(0);

    // Segment buttons drive the same query; 全部 removes it.
    await page.getByTestId("inbox-kind-question").click();
    await expect(page).toHaveURL(/kind=question$/);
    await page.getByTestId("inbox-kind-all").click();
    await expect(page).toHaveURL(/\/m\/inbox$/);
    await expect(page.getByTestId("approval-row")).toHaveCount(2);
  });
  test("kind radiogroup is fully 44px tappable at the coarse band edges", async ({
    page
  }) => {
    // UO-9 round-2: the shared segItem is 26px VISIBLE and its 44px reach is
    // the ::after band 9px above/below it. Inside an overflow-x scrollport
    // that band must stay hittable (kept inside the track's block padding,
    // not an outer margin the scrollport clips). Tap 2px inside the top and
    // bottom of the 44px band — both points are OUTSIDE the 26px item.
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    const group = page.getByRole("radiogroup", {
      name: "交互类型"
    });
    await expect(group).toBeVisible();
    const band = await group.boundingBox();
    expect(band, "kind track rendered").toBeTruthy();
    expect(band.height).toBeGreaterThanOrEqual(43);
    const testIdAt = async (x, y) => page.evaluate(({
      x,
      y
    }) => {
      var _el$dataset$testid;
      const el = document.elementFromPoint(x, y);
      return el ? (_el$dataset$testid = el.dataset.testid) !== null && _el$dataset$testid !== void 0 ? _el$dataset$testid : el.tagName : null;
    }, {
      x,
      y
    });
    const approval = page.getByTestId("inbox-kind-approval");
    const ab = await approval.boundingBox();
    expect(ab).toBeTruthy();
    const cx = ab.x + ab.width / 2;
    const topY = band.y + 1 + 2; // 2px inside the band top (above the item)
    const bottomY = band.y + band.height - 1 - 2; // 2px inside the band bottom
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
    await page.mouse.click(allBox.x + allBox.width / 2, bottomY);
    await expect(page).toHaveURL(/\/m\/inbox$/);
  });
  test("a Node-restarted session is filtered out of both compact inbox tiers; its session banner is neutral", async ({
    page,
    browser
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
      viewport: {
        width: 1280,
        height: 800
      },
      hasTouch: false,
      isMobile: false
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
    await desktopContext.close().catch(() => undefined);

    // c-endreason r2: ui-spec §4.7 gives the compact inbox TWO tiers and no
    // third. The ended session is filtered out entirely; the live survivor
    // still anchors 进行中 · 最近.
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox")).toBeVisible();
    await expect(page.locator(`[data-testid="m-inbox-recent-row"][data-instance-id="${instanceId}"]`)).toHaveCount(0);
    // No ended section exists at all — not merely collapsed.
    await expect(page.getByTestId("m-inbox-ended")).toHaveCount(0);
    await expect(page.locator(`[data-testid="m-inbox-recent-row"][data-instance-id="${survivorId}"]`)).toHaveCount(1, {
      timeout: 15000
    });
    expect(await page.locator("[data-testid='m-inbox']").innerText()).not.toContain("node-epoch-changed");

    // The session page keeps the restart banner: neutral sentence, neutral
    // chrome (never the danger border/background role), Resume kept.
    await page.goto(`/s/${instanceId}`);
    const banner = page.getByTestId("node-restart-banner");
    await expect(banner).toContainText("Node 重启，会话已中断", {
      timeout: 20000
    });
    expect(await banner.innerText()).not.toContain("node-epoch-changed");
    await expect(banner).toHaveAttribute("title", "node-epoch-changed");
    await expect(page.getByTestId("node-restart-resume")).toBeVisible();
    const chrome = await banner.evaluate(el => {
      const resolve = (value, prop) => {
        const probe = document.createElement("div");
        probe.style.setProperty(prop, value);
        document.body.appendChild(probe);
        const resolved = getComputedStyle(probe).getPropertyValue(prop);
        probe.remove();
        return resolved;
      };
      const cs = getComputedStyle(el);
      return {
        border: cs.getPropertyValue("border-top-color"),
        background: cs.getPropertyValue("background-color"),
        dangerBorder: resolve("var(--danger-border)", "border-top-color"),
        dangerBg: resolve("var(--danger-bg)", "background-color"),
        neutralBorder: resolve("var(--border)", "border-top-color"),
        neutralBg: resolve("var(--bg-surface)", "background-color")
      };
    });
    expect(chrome.dangerBorder).not.toBe("");
    expect(chrome.border).not.toBe(chrome.dangerBorder);
    expect(chrome.background).not.toBe(chrome.dangerBg);
    // It is not just "a different red": the banner uses the neutral surface.
    expect(chrome.border).toBe(chrome.neutralBorder);
    expect(chrome.background).toBe(chrome.neutralBg);
    if (evidence) {
      const evidenceContext = await browser.newContext({
        viewport: {
          width: 1440,
          height: 900
        },
        hasTouch: false,
        isMobile: false
      });
      const evidencePage = await evidenceContext.newPage();
      await login(evidencePage, "m-inbox-ended-evidence");
      await evidencePage.goto(`/s/${instanceId}`);
      await expect(evidencePage.getByTestId("node-restart-banner")).toContainText("Node 重启，会话已中断", {
        timeout: 20000
      });
      await shot(evidencePage, "desktop-endreason-node-restart-1440.png");
      await evidenceContext.close();
    }
  });
  test("evidence: inbox banner and both tiers at 390px", async ({
    page
  }) => {
    test.skip(!evidence, "set REMUDA_EVIDENCE=1 to capture the committed screenshot");
    await page.emulateMedia({
      reducedMotion: "reduce"
    });
    // One answered session -> idle row in 进行中 · 最近; one still pending.
    const answeredInstance = await createSession(page, "m inbox evidence recent");
    const answeredInteraction = await pendingInteractionId(page, answeredInstance);
    await page.goto("/m/inbox");
    const answeredRow = inboxRow(page, answeredInteraction);
    await answeredRow.getByRole("button", {
      name: "允许一次"
    }).click();
    await expect(answeredRow).toHaveCount(0, {
      timeout: 20000
    });
    await createSession(page, "m inbox evidence pending");
    await page.goto("/m/inbox");
    await expect(page.getByTestId("m-inbox-push-banner")).toBeVisible({
      timeout: 10000
    });
    await expect(page.getByTestId("approval-row")).toHaveCount(1, {
      timeout: 15000
    });
    await expect(page.getByTestId("m-inbox-recent-row")).toHaveCount(1, {
      timeout: 15000
    });
    await shot(page, "mobile-ui-4-inbox-390.png");
  });
});
//# sourceMappingURL=data:application/json;charset=utf-8;base64,eyJ2ZXJzaW9uIjozLCJuYW1lcyI6WyJleHBlY3QiLCJ0ZXN0IiwibWtkaXIiLCJwYXRoIiwiZmlsZVVSTFRvUGF0aCIsImxvZ2luIiwiZGVzY3JpYmUiLCJjb25maWd1cmUiLCJtb2RlIiwiVklFV1BPUlRfSCIsImhlcmUiLCJkaXJuYW1lIiwiaW1wb3J0IiwibWV0YSIsInVybCIsImV2aWRlbmNlIiwicHJvY2VzcyIsImVudiIsIlJFTVVEQV9FVklERU5DRSIsInNob3REaXIiLCJqb2luIiwiY3JlYXRlZCIsInNob3QiLCJwYWdlIiwibmFtZSIsInJlY3Vyc2l2ZSIsInNjcmVlbnNob3QiLCJhbmltYXRpb25zIiwiY3JlYXRlU2Vzc2lvbiIsInByb21wdCIsIndvcmtzcGFjZUlkIiwiZ290byIsImhvc3RQaWNrZXIiLCJnZXRCeVRlc3RJZCIsInRvQ29udGFpblRleHQiLCJ0aW1lb3V0IiwiaG9zdElkIiwibG9jYXRvciIsImZpbHRlciIsImhhc1RleHQiLCJnZXRBdHRyaWJ1dGUiLCJ0b0JlVHJ1dGh5Iiwic2VsZWN0T3B0aW9uIiwiY2xpY2siLCJub3QiLCJ0b0hhdmVDb3VudCIsImZpbGwiLCJ3YWl0Rm9yVVJMIiwiaWQiLCJVUkwiLCJwYXRobmFtZSIsInNwbGl0IiwicG9wIiwicHVzaCIsInBlbmRpbmdJbnRlcmFjdGlvbklkIiwiaW5zdGFuY2VJZCIsImV2YWx1YXRlIiwiaWlkIiwiX2JvZHkkaXRlbXMiLCJfcmVmIiwiX2ZvdW5kJGludGVyYWN0aW9uSWQiLCJib2R5IiwiZmV0Y2giLCJjcmVkZW50aWFscyIsImpzb24iLCJmb3VuZCIsIml0ZW1zIiwiZmluZCIsIml0ZW0iLCJzdGF0ZSIsImludGVyYWN0aW9uSWQiLCJpbmJveFJvdyIsImNyZWF0ZVJ1bm5pbmdQdHkiLCJob3N0cyIsInJlcXVlc3QiLCJnZXQiLCJob3N0IiwibGFiZWwiLCJ3b3Jrc3BhY2VzIiwid29ya3NwYWNlIiwiY3JlYXRlIiwicG9zdCIsImhlYWRlcnMiLCJPcmlnaW4iLCJvcmlnaW4iLCJkYXRhIiwiY3dkIiwicm9vdCIsImtpbmQiLCJkcml2ZXIiLCJtb2RlbCIsIm9rIiwidGV4dCIsInRvQmUiLCJpbnN0YW5jZSIsInBvbGwiLCJsaWZlY3ljbGUiLCJyZXN0YXJ0Tm9kZUZyb21UdHkiLCJ0b0hhdmVBdHRyaWJ1dGUiLCJmaXJzdCIsImVsIiwiZm9jdXMiLCJrZXlib2FyZCIsInR5cGUiLCJwcmVzcyIsImV4cGVjdFNldHRsZWRCeVJlc3RhcnQiLCJsYXN0RXJyb3IiLCJ1c2UiLCJ2aWV3cG9ydCIsIndpZHRoIiwiaGVpZ2h0IiwiaGFzVG91Y2giLCJpc01vYmlsZSIsImJlZm9yZUVhY2giLCJhZnRlckVhY2giLCJ1bnJvdXRlQWxsIiwiYmVoYXZpb3IiLCJjYXRjaCIsInVuZGVmaW5lZCIsInNwbGljZSIsImRlbGV0ZSIsInRvQmVWaXNpYmxlIiwicm93IiwiYWxsb3ciLCJnZXRCeVJvbGUiLCJ0b0hhdmVUZXh0IiwicmVjZW50IiwiZ2V0QnlUZXh0Iiwicm91dGUiLCJfYm9keSRpdGVtczIiLCJfcmVzcG9uc2UkaGVhZGVycyRjb24iLCJyZXNwb25zZSIsInBhdGNoZWQiLCJtYXAiLCJmdWxmaWxsIiwic3RhdHVzIiwiY29udGVudFR5cGUiLCJKU09OIiwic3RyaW5naWZ5IiwidG9CZURpc2FibGVkIiwicGFpcnMiLCJpIiwiaW50ZXJhY3Rpb24iLCJ0YXJnZXQiLCJmb2N1c2VkIiwiYm94IiwiYm91bmRpbmdCb3giLCJ5IiwiYm90dG9tIiwidG9NYXRjaE9iamVjdCIsImFueSIsIk51bWJlciIsInRvQmVHcmVhdGVyVGhhbk9yRXF1YWwiLCJ0b0JlTGVzc1RoYW5PckVxdWFsIiwieCIsImFwcHJvdmFsSW5zdGFuY2UiLCJxdWVzdGlvbkluc3RhbmNlIiwicXVlc3Rpb25JZCIsInFyb3ciLCJhbnN3ZXIiLCJ0b0hhdmVVUkwiLCJncm91cCIsImJhbmQiLCJ0ZXN0SWRBdCIsIl9lbCRkYXRhc2V0JHRlc3RpZCIsImRvY3VtZW50IiwiZWxlbWVudEZyb21Qb2ludCIsImRhdGFzZXQiLCJ0ZXN0aWQiLCJ0YWdOYW1lIiwiYXBwcm92YWwiLCJhYiIsImN4IiwidG9wWSIsImJvdHRvbVkiLCJtb3VzZSIsImFsbCIsImFsbEJveCIsImJyb3dzZXIiLCJzbG93IiwiZGVza3RvcENvbnRleHQiLCJuZXdDb250ZXh0IiwiZGVza3RvcCIsIm5ld1BhZ2UiLCJzdXJ2aXZvcklkIiwiY2xvc2UiLCJpbm5lclRleHQiLCJ0b0NvbnRhaW4iLCJiYW5uZXIiLCJjaHJvbWUiLCJyZXNvbHZlIiwidmFsdWUiLCJwcm9wIiwicHJvYmUiLCJjcmVhdGVFbGVtZW50Iiwic3R5bGUiLCJzZXRQcm9wZXJ0eSIsImFwcGVuZENoaWxkIiwicmVzb2x2ZWQiLCJnZXRDb21wdXRlZFN0eWxlIiwiZ2V0UHJvcGVydHlWYWx1ZSIsInJlbW92ZSIsImNzIiwiYm9yZGVyIiwiYmFja2dyb3VuZCIsImRhbmdlckJvcmRlciIsImRhbmdlckJnIiwibmV1dHJhbEJvcmRlciIsIm5ldXRyYWxCZyIsImV2aWRlbmNlQ29udGV4dCIsImV2aWRlbmNlUGFnZSIsInNraXAiLCJlbXVsYXRlTWVkaWEiLCJyZWR1Y2VkTW90aW9uIiwiYW5zd2VyZWRJbnN0YW5jZSIsImFuc3dlcmVkSW50ZXJhY3Rpb24iLCJhbnN3ZXJlZFJvdyJdLCJzb3VyY2VzIjpbIm0taW5ib3guaHViLnNwZWMudHMiXSwic291cmNlc0NvbnRlbnQiOlsiaW1wb3J0IHsgZXhwZWN0LCB0ZXN0LCB0eXBlIFBhZ2UgfSBmcm9tIFwiQHBsYXl3cmlnaHQvdGVzdFwiO1xuaW1wb3J0IHsgbWtkaXIgfSBmcm9tIFwibm9kZTpmcy9wcm9taXNlc1wiO1xuaW1wb3J0IHBhdGggZnJvbSBcIm5vZGU6cGF0aFwiO1xuaW1wb3J0IHsgZmlsZVVSTFRvUGF0aCB9IGZyb20gXCJub2RlOnVybFwiO1xuaW1wb3J0IHsgbG9naW4gfSBmcm9tIFwiLi9odWItYXV0aFwiO1xuXG4vKipcbiAqIGMtbWluYm94IC8gRC0wNDkgdWktc3BlYyDCpzIuNSDCpzQuNzogdGhlIHBob25lIGluYm94IGF0IC9tL2luYm94LlxuICpcbiAqIDM5MHB4IGh1YiAoZmFrZSBOb2RlKSBjb3ZlcmFnZTpcbiAqICAtIHRoZSBmYWtlIG5vZGUgcmFpc2VzIG9uZSBhcHByb3ZhbCBwZXIgZnJlc2ggc2Vzc2lvbjsg5YWB6K645LiA5qyhIHJlbW92ZXMgdGhlXG4gKiAgICByb3cgKGpvdXJuYWwgcmVjZWlwdCkgYW5kIG5vIHNlY29uZCBzdWJtaXQgaXMgcG9zc2libGU7XG4gKiAgLSBhbiBvZmZsaW5lIGhvc3QgcGF1c2VzIHRoZSByb3c6IGV2ZXJ5IG9wdGlvbiBidXR0b24gaXMgZGlzYWJsZWQgYW5kIHRoZVxuICogICAgcm93IGNhcnJpZXMg5Li75py656a757q/77yM5Lqk5LqS5pqC5YGcO1xuICogIC0gP2ZvY3VzPTxpbnRlcmFjdGlvbklkPiBzY3JvbGxzIHRoYXQgcm93IGludG8gdGhlIHZpZXdwb3J0IGFuZCBtYXJrcyBpdDtcbiAqICAtID9raW5kPSBmaWx0ZXJzIHdpdGggdGhlIHNhbWUgcXVlcnkgc2VtYW50aWNzIGFzIC9hcHByb3ZhbHMsIGFuZCBxdWVzdGlvblxuICogICAgcm93cyBvZmZlciDljrvlm57nrZQgdG8gL3MvOmlkIGluc3RlYWQgb2YgaW5saW5lIGZvcm1zLlxuICovXG50ZXN0LmRlc2NyaWJlLmNvbmZpZ3VyZSh7IG1vZGU6IFwic2VyaWFsXCIgfSk7XG5cbmNvbnN0IFZJRVdQT1JUX0ggPSA4NDQ7XG5cbmNvbnN0IGhlcmUgPSBwYXRoLmRpcm5hbWUoZmlsZVVSTFRvUGF0aChpbXBvcnQubWV0YS51cmwpKTtcbmNvbnN0IGV2aWRlbmNlID0gcHJvY2Vzcy5lbnYuUkVNVURBX0VWSURFTkNFID09PSBcIjFcIjtcbmNvbnN0IHNob3REaXIgPSBldmlkZW5jZVxuICA/IHBhdGguam9pbihoZXJlLCBcIi4uLy4uLy4uL2RvY3MvZGVzaWduL2V2aWRlbmNlXCIpXG4gIDogcGF0aC5qb2luKGhlcmUsIFwiLi4vLi4vdGVzdC1yZXN1bHRzL2V2aWRlbmNlXCIpO1xuXG5jb25zdCBjcmVhdGVkOiBzdHJpbmdbXSA9IFtdO1xuXG5hc3luYyBmdW5jdGlvbiBzaG90KHBhZ2U6IFBhZ2UsIG5hbWU6IHN0cmluZykge1xuICBhd2FpdCBta2RpcihzaG90RGlyLCB7IHJlY3Vyc2l2ZTogdHJ1ZSB9KTtcbiAgYXdhaXQgcGFnZS5zY3JlZW5zaG90KHsgcGF0aDogcGF0aC5qb2luKHNob3REaXIsIG5hbWUpLCBhbmltYXRpb25zOiBcImRpc2FibGVkXCIgfSk7XG59XG5cbmFzeW5jIGZ1bmN0aW9uIGNyZWF0ZVNlc3Npb24ocGFnZTogUGFnZSwgcHJvbXB0OiBzdHJpbmcsIHdvcmtzcGFjZUlkPzogc3RyaW5nKTogUHJvbWlzZTxzdHJpbmc+IHtcbiAgYXdhaXQgcGFnZS5nb3RvKFwiL3Nlc3Npb25zL25ld1wiKTtcbiAgY29uc3QgaG9zdFBpY2tlciA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1ob3N0XCIpO1xuICBhd2FpdCBleHBlY3QoaG9zdFBpY2tlcikudG9Db250YWluVGV4dChcImUyZS1mYWtlLW5vZGVcIiwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGhvc3RJZCA9IGF3YWl0IGhvc3RQaWNrZXJcbiAgICAubG9jYXRvcihcIm9wdGlvblwiKVxuICAgIC5maWx0ZXIoeyBoYXNUZXh0OiBcImUyZS1mYWtlLW5vZGVcIiB9KVxuICAgIC5nZXRBdHRyaWJ1dGUoXCJ2YWx1ZVwiKTtcbiAgZXhwZWN0KGhvc3RJZCkudG9CZVRydXRoeSgpO1xuICBhd2FpdCBob3N0UGlja2VyLnNlbGVjdE9wdGlvbihob3N0SWQhKTtcbiAgYXdhaXQgcGFnZS5nZXRCeVRlc3RJZChcIm5ldy1zZXNzaW9uLWtpbmQtY2xhdWRlXCIpLmNsaWNrKCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLmxvY2F0b3IoXCJvcHRpb25cIikpLm5vdC50b0hhdmVDb3VudCgwLCB7XG4gICAgdGltZW91dDogMjBfMDAwLFxuICB9KTtcbiAgaWYgKHdvcmtzcGFjZUlkKSBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24td29ya3NwYWNlXCIpLnNlbGVjdE9wdGlvbih3b3Jrc3BhY2VJZCk7XG4gIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJuZXctc2Vzc2lvbi1wcm9tcHRcIikuZmlsbChwcm9tcHQpO1xuICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwibmV3LXNlc3Npb24tc3RhcnRcIikuY2xpY2soKTtcbiAgYXdhaXQgcGFnZS53YWl0Rm9yVVJMKC9cXC9zXFwvLywgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gIGNvbnN0IGlkID0gbmV3IFVSTChwYWdlLnVybCgpKS5wYXRobmFtZS5zcGxpdChcIi9cIikucG9wKCkhO1xuICBjcmVhdGVkLnB1c2goaWQpO1xuICByZXR1cm4gaWQ7XG59XG5cbmFzeW5jIGZ1bmN0aW9uIHBlbmRpbmdJbnRlcmFjdGlvbklkKHBhZ2U6IFBhZ2UsIGluc3RhbmNlSWQ6IHN0cmluZyk6IFByb21pc2U8c3RyaW5nPiB7XG4gIGNvbnN0IGlkID0gYXdhaXQgcGFnZS5ldmFsdWF0ZShhc3luYyAoaWlkKSA9PiB7XG4gICAgY29uc3QgYm9keSA9IGF3YWl0IChhd2FpdCBmZXRjaChcIi92MS9pbnRlcmFjdGlvbnNcIiwgeyBjcmVkZW50aWFsczogXCJpbmNsdWRlXCIgfSkpLmpzb24oKTtcbiAgICBjb25zdCBmb3VuZCA9IChib2R5Lml0ZW1zID8/IFtdKS5maW5kKFxuICAgICAgKGl0ZW06IHsgaW5zdGFuY2VJZD86IHN0cmluZzsgc3RhdGU/OiBzdHJpbmc7IGludGVyYWN0aW9uSWQ/OiBzdHJpbmc7IGlkPzogc3RyaW5nIH0pID0+XG4gICAgICAgIGl0ZW0uaW5zdGFuY2VJZCA9PT0gaWlkICYmIGl0ZW0uc3RhdGUgPT09IFwicGVuZGluZ1wiLFxuICAgICk7XG4gICAgcmV0dXJuIGZvdW5kPy5pbnRlcmFjdGlvbklkID8/IGZvdW5kPy5pZCA/PyBudWxsO1xuICB9LCBpbnN0YW5jZUlkKTtcbiAgZXhwZWN0KGlkLCBcInRoZSBmYWtlIG5vZGUgcmFpc2VzIGEgcGVuZGluZyBhcHByb3ZhbCBmb3IgYSBmcmVzaCBzZXNzaW9uXCIpLnRvQmVUcnV0aHkoKTtcbiAgcmV0dXJuIGlkIGFzIHN0cmluZztcbn1cblxuZnVuY3Rpb24gaW5ib3hSb3cocGFnZTogUGFnZSwgaW50ZXJhY3Rpb25JZDogc3RyaW5nKSB7XG4gIHJldHVybiBwYWdlLmxvY2F0b3IoYFtkYXRhLWludGVyYWN0aW9uLWlkPVwiJHtpbnRlcmFjdGlvbklkfVwiXWApO1xufVxuXG4vKipcbiAqIEEgYGNsYXVkZS1wdHlgIHNlc3Npb24gY3JlYXRlZCBzdHJhaWdodCB0aHJvdWdoIHRoZSBSRVNUIEFQSTogaXQgcmVhY2hlc1xuICogYHJ1bm5pbmdgIChzbyBpdCBiZWxvbmdzIHRvIOi/m+ihjOS4rSDCtyDmnIDov5EpIGFuZCBpdHMgdHR5IGFjY2VwdHNcbiAqIFRUWU5PREVfUkVTVEFSVCwgdGhlIGZha2UgTm9kZSdzIG93biBlcG9jaC1idW1wIGZpeHR1cmUuXG4gKi9cbmFzeW5jIGZ1bmN0aW9uIGNyZWF0ZVJ1bm5pbmdQdHkocGFnZTogUGFnZSk6IFByb21pc2U8c3RyaW5nPiB7XG4gIGNvbnN0IGhvc3RzID0gKGF3YWl0IChhd2FpdCBwYWdlLnJlcXVlc3QuZ2V0KFwiL3YxL2hvc3RzXCIpKS5qc29uKCkpIGFzIHtcbiAgICBpdGVtczogeyBob3N0SWQ6IHN0cmluZzsgbGFiZWw6IHN0cmluZyB9W107XG4gIH07XG4gIGNvbnN0IGhvc3QgPSBob3N0cy5pdGVtcy5maW5kKChpdGVtKSA9PiBpdGVtLmxhYmVsID09PSBcImUyZS1mYWtlLW5vZGVcIik7XG4gIGV4cGVjdChob3N0KS50b0JlVHJ1dGh5KCk7XG4gIGNvbnN0IHdvcmtzcGFjZXMgPSAoYXdhaXQgKFxuICAgIGF3YWl0IHBhZ2UucmVxdWVzdC5nZXQoYC92MS9ob3N0cy8ke2hvc3QhLmhvc3RJZH0vd29ya3NwYWNlc2ApXG4gICkuanNvbigpKSBhcyB7IHdvcmtzcGFjZXM6IHsgd29ya3NwYWNlSWQ6IHN0cmluZzsgcm9vdDogc3RyaW5nIH1bXSB9O1xuICBjb25zdCB3b3Jrc3BhY2UgPSB3b3Jrc3BhY2VzLndvcmtzcGFjZXNbMF07XG4gIGV4cGVjdCh3b3Jrc3BhY2UpLnRvQmVUcnV0aHkoKTtcbiAgY29uc3QgY3JlYXRlID0gYXdhaXQgcGFnZS5yZXF1ZXN0LnBvc3QoXCIvdjEvaW5zdGFuY2VzXCIsIHtcbiAgICBoZWFkZXJzOiB7IE9yaWdpbjogbmV3IFVSTChwYWdlLnVybCgpKS5vcmlnaW4gfSxcbiAgICBkYXRhOiB7XG4gICAgICBob3N0SWQ6IGhvc3QhLmhvc3RJZCxcbiAgICAgIHdvcmtzcGFjZUlkOiB3b3Jrc3BhY2Uud29ya3NwYWNlSWQsXG4gICAgICBjd2Q6IHdvcmtzcGFjZS5yb290LFxuICAgICAga2luZDogXCJjbGF1ZGVcIixcbiAgICAgIGRyaXZlcjogXCJjbGF1ZGUtcHR5XCIsXG4gICAgICBtb2RlbDogXCJlMmUvYXV0b1wiLFxuICAgICAgbmFtZTogXCJlMmUtbS1pbmJveC1yZXN0YXJ0XCIsXG4gICAgfSxcbiAgfSk7XG4gIGV4cGVjdChjcmVhdGUub2soKSwgYXdhaXQgY3JlYXRlLnRleHQoKSkudG9CZSh0cnVlKTtcbiAgY29uc3QgaW5zdGFuY2VJZCA9ICgoYXdhaXQgY3JlYXRlLmpzb24oKSkgYXMgeyBpbnN0YW5jZTogeyBpbnN0YW5jZUlkOiBzdHJpbmcgfSB9KS5pbnN0YW5jZVxuICAgIC5pbnN0YW5jZUlkO1xuICBjcmVhdGVkLnB1c2goaW5zdGFuY2VJZCk7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT5cbiAgICAgICAgKChhd2FpdCAoYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCkpLmpzb24oKSkgYXMge1xuICAgICAgICAgIGxpZmVjeWNsZT86IHN0cmluZztcbiAgICAgICAgfSkubGlmZWN5Y2xlLFxuICAgICAgeyB0aW1lb3V0OiAyMF8wMDAgfSxcbiAgICApXG4gICAgLnRvQmUoXCJydW5uaW5nXCIpO1xuICByZXR1cm4gaW5zdGFuY2VJZDtcbn1cblxuYXN5bmMgZnVuY3Rpb24gcmVzdGFydE5vZGVGcm9tVHR5KHBhZ2U6IFBhZ2UsIGluc3RhbmNlSWQ6IHN0cmluZyk6IFByb21pc2U8dm9pZD4ge1xuICBhd2FpdCBwYWdlLmdvdG8oYC9zLyR7aW5zdGFuY2VJZH0vdHR5YCk7XG4gIGF3YWl0IGV4cGVjdChwYWdlLmxvY2F0b3IoXCJbZGF0YS10dHktbGFiPScxJ11cIikpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtdHR5LXN0YXR1c1wiLCBcImxpdmVcIiwge1xuICAgIHRpbWVvdXQ6IDMwXzAwMCxcbiAgfSk7XG4gIGF3YWl0IHBhZ2VcbiAgICAubG9jYXRvcihcIi54dGVybS1oZWxwZXItdGV4dGFyZWFcIilcbiAgICAuZmlyc3QoKVxuICAgIC5ldmFsdWF0ZSgoZWwpID0+IChlbCBhcyBIVE1MVGV4dEFyZWFFbGVtZW50KS5mb2N1cygpKTtcbiAgYXdhaXQgcGFnZS5rZXlib2FyZC50eXBlKFwiVFRZTk9ERV9SRVNUQVJUXCIpO1xuICBhd2FpdCBwYWdlLmtleWJvYXJkLnByZXNzKFwiRW50ZXJcIik7XG59XG5cbmFzeW5jIGZ1bmN0aW9uIGV4cGVjdFNldHRsZWRCeVJlc3RhcnQocGFnZTogUGFnZSwgaW5zdGFuY2VJZDogc3RyaW5nKTogUHJvbWlzZTx2b2lkPiB7XG4gIGF3YWl0IGV4cGVjdFxuICAgIC5wb2xsKFxuICAgICAgYXN5bmMgKCkgPT5cbiAgICAgICAgKChhd2FpdCAoYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCkpLmpzb24oKSkgYXMge1xuICAgICAgICAgIGxpZmVjeWNsZT86IHN0cmluZztcbiAgICAgICAgICBsYXN0RXJyb3I/OiBzdHJpbmc7XG4gICAgICAgIH0pLmxpZmVjeWNsZSxcbiAgICAgIHsgdGltZW91dDogMzBfMDAwIH0sXG4gICAgKVxuICAgIC50b0JlKFwiZXhpdGVkXCIpO1xuICBleHBlY3QoXG4gICAgKChhd2FpdCAoYXdhaXQgcGFnZS5yZXF1ZXN0LmdldChgL3YxL2luc3RhbmNlcy8ke2luc3RhbmNlSWR9YCkpLmpzb24oKSkgYXMge1xuICAgICAgbGFzdEVycm9yPzogc3RyaW5nO1xuICAgIH0pLmxhc3RFcnJvcixcbiAgKS50b0JlKFwibm9kZS1lcG9jaC1jaGFuZ2VkXCIpO1xufVxuXG50ZXN0LmRlc2NyaWJlKFwiMzkwcHggcGhvbmUgaW5ib3hcIiwgKCkgPT4ge1xuICB0ZXN0LnVzZSh7XG4gICAgdmlld3BvcnQ6IHsgd2lkdGg6IDM5MCwgaGVpZ2h0OiBWSUVXUE9SVF9IIH0sXG4gICAgaGFzVG91Y2g6IHRydWUsXG4gICAgaXNNb2JpbGU6IHRydWUsXG4gIH0pO1xuXG4gIHRlc3QuYmVmb3JlRWFjaChhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgICBhd2FpdCBsb2dpbihwYWdlKTtcbiAgfSk7XG5cbiAgdGVzdC5hZnRlckVhY2goYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gICAgLy8gQSAyIHMgc3RvcmUgcG9sbCBjYW4gc3RpbGwgYmUgaW5zaWRlIHRoZSBvZmZsaW5lIHRlc3QncyByb3V0ZSBjYWxsYmFja1xuICAgIC8vIGF0IHRlYXJkb3duOyBkcm9wIHRoZSBpbnRlcmNlcHRpb24gYmVmb3JlIHRoZSBuZXh0IHRlc3QgbW91bnRzLlxuICAgIGF3YWl0IHBhZ2UudW5yb3V0ZUFsbCh7IGJlaGF2aW9yOiBcImlnbm9yZUVycm9yc1wiIH0pLmNhdGNoKCgpID0+IHVuZGVmaW5lZCk7XG4gICAgZm9yIChjb25zdCBpZCBvZiBjcmVhdGVkLnNwbGljZSgwKSkge1xuICAgICAgYXdhaXQgcGFnZS5yZXF1ZXN0LmRlbGV0ZShgL3YxL2luc3RhbmNlcy8ke2lkfT9mb3JjZT0xYCkuY2F0Y2goKCkgPT4gdW5kZWZpbmVkKTtcbiAgICB9XG4gIH0pO1xuXG4gIHRlc3QoXCLlhYHorrjkuIDmrKEgcmVtb3ZlcyB0aGUgcm93IGFuZCBhIHNlY29uZCBzdWJtaXQgaXMgaW1wb3NzaWJsZVwiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgICBjb25zdCBpbnN0YW5jZUlkID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBcIm0gaW5ib3ggYWxsb3cgb25jZVwiKTtcbiAgICBjb25zdCBpbnRlcmFjdGlvbklkID0gYXdhaXQgcGVuZGluZ0ludGVyYWN0aW9uSWQocGFnZSwgaW5zdGFuY2VJZCk7XG5cbiAgICBhd2FpdCBwYWdlLmdvdG8oXCIvbS9pbmJveFwiKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm0taW5ib3hcIikpLnRvQmVWaXNpYmxlKCk7XG4gICAgY29uc3Qgcm93ID0gaW5ib3hSb3cocGFnZSwgaW50ZXJhY3Rpb25JZCk7XG4gICAgYXdhaXQgZXhwZWN0KHJvdykudG9Db250YWluVGV4dChcImVjaG8gZTJlXCIpO1xuICAgIGNvbnN0IGFsbG93ID0gcm93LmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwi5YWB6K645LiA5qyhXCIgfSk7XG4gICAgYXdhaXQgZXhwZWN0KGFsbG93KS50b0JlVmlzaWJsZSgpO1xuICAgIGF3YWl0IGV4cGVjdChyb3cuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogXCLmi5Lnu51cIiB9KSkudG9CZVZpc2libGUoKTtcbiAgICBhd2FpdCBleHBlY3Qocm93LmdldEJ5Um9sZShcImxpbmtcIiwgeyBuYW1lOiBcIuaJk+W8gOS8muivnVwiIH0pKS50b0JlVmlzaWJsZSgpO1xuXG4gICAgYXdhaXQgYWxsb3cuY2xpY2soKTtcbiAgICAvLyBUaGUgb3B0aW9uIGJ1dHRvbnMgbGVhdmUgdGhlIHJvdyBpbW1lZGlhdGVseSAoYW5zd2VyaW5nIC0+IOW3suaPkOS6pCwgbm9cbiAgICAvLyBsb2NhbCBsb2NrIGdyYWIpOyB0aGUgcm93IGl0c2VsZiBkaXNhcHBlYXJzIHdoZW4gdGhlIGpvdXJuYWwgcmVjZWlwdFxuICAgIC8vIGxhbmRzLiBUaGVyZSBpcyBubyBjb250cm9sIGxlZnQgdGhhdCBjb3VsZCBzdWJtaXQgYSBzZWNvbmQgYW5zd2VyLlxuICAgIGF3YWl0IGV4cGVjdChyb3cpLnRvSGF2ZUNvdW50KDAsIHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwi5YWB6K645LiA5qyhXCIgfSkpLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibS1pbmJveC1wZW5kaW5nLWNvdW50XCIpKS50b0hhdmVUZXh0KFwi5b6F5aSE55CGIDBcIik7XG5cbiAgICAvLyBUaGUgYW5zd2VyZWQgaW5zdGFuY2UgdHVybnMgaWRsZSBhbmQgbW92ZXMgdG8g6L+b6KGM5LitIMK3IOacgOi/kSDigJQgbmV2ZXIgYVxuICAgIC8vIHRoaXJkIHRpZXIuXG4gICAgY29uc3QgcmVjZW50ID0gcGFnZS5sb2NhdG9yKGBbZGF0YS1pbnN0YW5jZS1pZD1cIiR7aW5zdGFuY2VJZH1cIl1bZGF0YS10ZXN0aWQ9XCJtLWluYm94LXJlY2VudC1yb3dcIl1gKTtcbiAgICBhd2FpdCBleHBlY3QocmVjZW50KS50b0JlVmlzaWJsZSh7IHRpbWVvdXQ6IDEwXzAwMCB9KTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm0taW5ib3gtdGllci1yZWNlbnRcIikpLnRvQ29udGFpblRleHQoXCLov5vooYzkuK0gwrcg5pyA6L+RXCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGV4dChcIuW3suemu+mYn1wiKSkudG9IYXZlQ291bnQoMCk7XG4gIH0pO1xuXG4gIHRlc3QoXCJhbiBvZmZsaW5lIGhvc3QgZGlzYWJsZXMgdGhlIGJ1dHRvbnMgYW5kIHNob3dzIOS4u+acuuemu+e6v++8jOS6pOS6kuaaguWBnFwiLCBhc3luYyAoeyBwYWdlIH0pID0+IHtcbiAgICBjb25zdCBpbnN0YW5jZUlkID0gYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBcIm0gaW5ib3ggcGF1c2VkIGhvc3RcIik7XG4gICAgY29uc3QgaW50ZXJhY3Rpb25JZCA9IGF3YWl0IHBlbmRpbmdJbnRlcmFjdGlvbklkKHBhZ2UsIGluc3RhbmNlSWQpO1xuXG4gICAgLy8gVGhlIHNoYXJlZCBmYWtlIE5vZGUga2VlcHMgc2VydmluZyBvdGhlciBzcGVjczsgZW11bGF0ZSB0aGlzIGhvc3QgZ29pbmdcbiAgICAvLyBvZmZsaW5lIGF0IHRoZSBSRVNUIGJvdW5kYXJ5IChzYW1lIGFwcHJvYWNoIGFzIHV4LWZpbGVzLmh1Yi5zcGVjLnRzKS5cbiAgICBhd2FpdCBwYWdlLnJvdXRlKFwiKiovdjEvaG9zdHNcIiwgYXN5bmMgKHJvdXRlKSA9PiB7XG4gICAgICBjb25zdCByZXNwb25zZSA9IGF3YWl0IHJvdXRlLmZldGNoKCk7XG4gICAgICBjb25zdCBib2R5ID0gYXdhaXQgcmVzcG9uc2UuanNvbigpO1xuICAgICAgY29uc3QgcGF0Y2hlZCA9IHtcbiAgICAgICAgLi4uYm9keSxcbiAgICAgICAgaXRlbXM6IChib2R5Lml0ZW1zID8/IFtdKS5tYXAoKGl0ZW06IFJlY29yZDxzdHJpbmcsIHVua25vd24+KSA9PlxuICAgICAgICAgIGl0ZW0uc3RhdGUgPT09IFwib25saW5lXCIgfHwgaXRlbS5zdGF0ZSA9PT0gXCJlbnJvbGxlZFwiXG4gICAgICAgICAgICA/IHsgLi4uaXRlbSwgc3RhdGU6IFwib2ZmbGluZVwiIH1cbiAgICAgICAgICAgIDogaXRlbSxcbiAgICAgICAgKSxcbiAgICAgIH07XG4gICAgICBhd2FpdCByb3V0ZS5mdWxmaWxsKHtcbiAgICAgICAgc3RhdHVzOiByZXNwb25zZS5zdGF0dXMoKSxcbiAgICAgICAgY29udGVudFR5cGU6IHJlc3BvbnNlLmhlYWRlcnMoKVtcImNvbnRlbnQtdHlwZVwiXSA/PyBcImFwcGxpY2F0aW9uL2pzb25cIixcbiAgICAgICAgYm9keTogSlNPTi5zdHJpbmdpZnkocGF0Y2hlZCksXG4gICAgICB9KTtcbiAgICB9KTtcblxuICAgIGF3YWl0IHBhZ2UuZ290byhcIi9tL2luYm94XCIpO1xuICAgIGNvbnN0IHJvdyA9IGluYm94Um93KHBhZ2UsIGludGVyYWN0aW9uSWQpO1xuICAgIGF3YWl0IGV4cGVjdChyb3cuZ2V0QnlUZXN0SWQoXCJtLWluYm94LXBhdXNlZFwiKSkudG9IYXZlVGV4dChcIuS4u+acuuemu+e6v++8jOS6pOS6kuaaguWBnFwiLCB7XG4gICAgICB0aW1lb3V0OiAxNV8wMDAsXG4gICAgfSk7XG4gICAgYXdhaXQgZXhwZWN0KHJvdy5nZXRCeVJvbGUoXCJidXR0b25cIiwgeyBuYW1lOiBcIuWFgeiuuOS4gOasoVwiIH0pKS50b0JlRGlzYWJsZWQoKTtcbiAgICBhd2FpdCBleHBlY3Qocm93LmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwi5ouS57udXCIgfSkpLnRvQmVEaXNhYmxlZCgpO1xuICAgIC8vIFRoZSBvcGVuLXNlc3Npb24gbGluayBpcyBub3QgYSBzdWJtaXQgcGF0aCBhbmQgc3RheXMgcmVhY2hhYmxlLlxuICAgIGF3YWl0IGV4cGVjdChyb3cuZ2V0QnlSb2xlKFwibGlua1wiLCB7IG5hbWU6IFwi5omT5byA5Lya6K+dXCIgfSkpLnRvQmVWaXNpYmxlKCk7XG4gIH0pO1xuXG4gIHRlc3QoXCI/Zm9jdXM9IGhpZ2hsaWdodHMgdGhlIHJvdyBhbmQgc2Nyb2xscyBpdCBpbnNpZGUgdGhlIHZpZXdwb3J0XCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICAgIC8vIEZvdXIgcGVuZGluZyByb3dzOiB0aGUgb2xkZXN0IGludGVyYWN0aW9uIHNvcnRzIHRvIHRoZSBib3R0b20gb2Yg5b6F5L2g5aSE55CGXG4gICAgLy8gYW5kIHN0YXJ0cyB3ZWxsIGJlbG93IHRoZSA4NDRweCBmb2xkLlxuICAgIGNvbnN0IHBhaXJzOiBBcnJheTx7IGluc3RhbmNlOiBzdHJpbmc7IGludGVyYWN0aW9uOiBzdHJpbmcgfT4gPSBbXTtcbiAgICBmb3IgKGxldCBpID0gMDsgaSA8IDQ7IGkgKz0gMSkge1xuICAgICAgY29uc3QgaW5zdGFuY2UgPSBhd2FpdCBjcmVhdGVTZXNzaW9uKHBhZ2UsIGBtIGluYm94IGZvY3VzICR7aX1gKTtcbiAgICAgIHBhaXJzLnB1c2goeyBpbnN0YW5jZSwgaW50ZXJhY3Rpb246IGF3YWl0IHBlbmRpbmdJbnRlcmFjdGlvbklkKHBhZ2UsIGluc3RhbmNlKSB9KTtcbiAgICB9XG4gICAgY29uc3QgdGFyZ2V0ID0gcGFpcnNbMF0uaW50ZXJhY3Rpb247XG5cbiAgICBhd2FpdCBwYWdlLmdvdG8oYC9tL2luYm94P2ZvY3VzPSR7dGFyZ2V0fWApO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiYXBwcm92YWwtcm93XCIpKS50b0hhdmVDb3VudCg0LCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgICBjb25zdCBmb2N1c2VkID0gcGFnZS5sb2NhdG9yKCdbZGF0YS1pbnRlcmFjdGlvbi1pZD1cIicgKyB0YXJnZXQgKyAnXCJdJyk7XG4gICAgYXdhaXQgZXhwZWN0KGZvY3VzZWQpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEtZm9jdXNcIiwgXCJ0cnVlXCIpO1xuXG4gICAgLy8gVGhlIHJvdydzIGJvcmRlciBib3ggc2l0cyBpbnNpZGUgdGhlIDM5MHg4NDQgdmlld3BvcnQgYWZ0ZXIgdGhlIGRlZXBcbiAgICAvLyBsaW5rIHNjcm9sbCAoYmxvY2s6Y2VudGVyIGxlYXZlcyBjbGVhciBtYXJnaW5zIG9uIGJvdGggc2lkZXMpLlxuICAgIGF3YWl0IGV4cGVjdFxuICAgICAgLnBvbGwoXG4gICAgICAgIGFzeW5jICgpID0+IHtcbiAgICAgICAgICBjb25zdCBib3ggPSBhd2FpdCBmb2N1c2VkLmJvdW5kaW5nQm94KCk7XG4gICAgICAgICAgaWYgKCFib3gpIHJldHVybiBudWxsO1xuICAgICAgICAgIHJldHVybiB7IHk6IGJveC55LCBib3R0b206IGJveC55ICsgYm94LmhlaWdodCB9O1xuICAgICAgICB9LFxuICAgICAgICB7IHRpbWVvdXQ6IDEwXzAwMCB9LFxuICAgICAgKVxuICAgICAgLnRvTWF0Y2hPYmplY3Qoe1xuICAgICAgICB5OiBleHBlY3QuYW55KE51bWJlciksXG4gICAgICAgIGJvdHRvbTogZXhwZWN0LmFueShOdW1iZXIpLFxuICAgICAgfSk7XG4gICAgY29uc3QgYm94ID0gYXdhaXQgZm9jdXNlZC5ib3VuZGluZ0JveCgpO1xuICAgIGV4cGVjdChib3gsIFwiZm9jdXNlZCByb3cgcmVuZGVyZWRcIikudG9CZVRydXRoeSgpO1xuICAgIGV4cGVjdChib3ghLnkpLnRvQmVHcmVhdGVyVGhhbk9yRXF1YWwoMCk7XG4gICAgZXhwZWN0KGJveCEueSArIGJveCEuaGVpZ2h0KS50b0JlTGVzc1RoYW5PckVxdWFsKFZJRVdQT1JUX0gpO1xuICAgIGV4cGVjdChib3ghLngpLnRvQmVHcmVhdGVyVGhhbk9yRXF1YWwoMCk7XG4gICAgZXhwZWN0KGJveCEueCArIGJveCEud2lkdGgpLnRvQmVMZXNzVGhhbk9yRXF1YWwoMzkwKTtcblxuICAgIC8vIE5vIGZvY3VzIHF1ZXJ5IC0+IG5vIHJvdyBpcyBtYXJrZWQuXG4gICAgYXdhaXQgcGFnZS5nb3RvKFwiL20vaW5ib3hcIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UubG9jYXRvcihcIltkYXRhLWZvY3VzPSd0cnVlJ11cIikpLnRvSGF2ZUNvdW50KDApO1xuICB9KTtcblxuICB0ZXN0KFwiP2tpbmQ9IGZpbHRlcnMgbGlrZSAvYXBwcm92YWxzIGFuZCBxdWVzdGlvbiByb3dzIGdvIOWOu+WbnuetlCB0byAvcy86aWRcIiwgYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gICAgY29uc3QgYXBwcm92YWxJbnN0YW5jZSA9IGF3YWl0IGNyZWF0ZVNlc3Npb24ocGFnZSwgXCJtIGluYm94IGtpbmQgYXBwcm92YWxcIik7XG4gICAgYXdhaXQgcGVuZGluZ0ludGVyYWN0aW9uSWQocGFnZSwgYXBwcm92YWxJbnN0YW5jZSk7XG4gICAgY29uc3QgcXVlc3Rpb25JbnN0YW5jZSA9IGF3YWl0IGNyZWF0ZVNlc3Npb24ocGFnZSwgXCJhc2stcXVlc3Rpb24gbSBpbmJveCBraW5kIHF1ZXN0aW9uXCIpO1xuICAgIGNvbnN0IHF1ZXN0aW9uSWQgPSBhd2FpdCBwZW5kaW5nSW50ZXJhY3Rpb25JZChwYWdlLCBxdWVzdGlvbkluc3RhbmNlKTtcblxuICAgIGF3YWl0IHBhZ2UuZ290byhcIi9tL2luYm94P2tpbmQ9cXVlc3Rpb25cIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtLWluYm94XCIpKS50b0JlVmlzaWJsZSgpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwiYXBwcm92YWwtcm93XCIpKS50b0hhdmVDb3VudCgxLCB7IHRpbWVvdXQ6IDIwXzAwMCB9KTtcbiAgICBjb25zdCBxcm93ID0gaW5ib3hSb3cocGFnZSwgcXVlc3Rpb25JZCk7XG4gICAgYXdhaXQgZXhwZWN0KHFyb3cpLnRvSGF2ZUF0dHJpYnV0ZShcImRhdGEta2luZFwiLCBcInF1ZXN0aW9uXCIpO1xuICAgIGNvbnN0IGFuc3dlciA9IHFyb3cuZ2V0QnlSb2xlKFwibGlua1wiLCB7IG5hbWU6IFwi5Y675Zue562UXCIgfSk7XG4gICAgYXdhaXQgZXhwZWN0KGFuc3dlcikudG9IYXZlQXR0cmlidXRlKFwiaHJlZlwiLCBgL3MvJHtxdWVzdGlvbkluc3RhbmNlfWApO1xuICAgIGF3YWl0IGV4cGVjdChxcm93LmdldEJ5Um9sZShcImJ1dHRvblwiLCB7IG5hbWU6IFwi5YWB6K645LiA5qyhXCIgfSkpLnRvSGF2ZUNvdW50KDApO1xuXG4gICAgYXdhaXQgcGFnZS5nb3RvKFwiL20vaW5ib3g/a2luZD1hcHByb3ZhbFwiKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcImFwcHJvdmFsLXJvd1wiKS5maWx0ZXIoeyBoYXNUZXh0OiBcImVjaG8gZTJlXCIgfSkpLnRvQmVWaXNpYmxlKHtcbiAgICAgIHRpbWVvdXQ6IDEwXzAwMCxcbiAgICB9KTtcbiAgICBhd2FpdCBleHBlY3QocGFnZS5sb2NhdG9yKGBbZGF0YS1pbnRlcmFjdGlvbi1pZD1cIiR7cXVlc3Rpb25JZH1cIl1gKSkudG9IYXZlQ291bnQoMCk7XG5cbiAgICAvLyBTZWdtZW50IGJ1dHRvbnMgZHJpdmUgdGhlIHNhbWUgcXVlcnk7IOWFqOmDqCByZW1vdmVzIGl0LlxuICAgIGF3YWl0IHBhZ2UuZ2V0QnlUZXN0SWQoXCJpbmJveC1raW5kLXF1ZXN0aW9uXCIpLmNsaWNrKCk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UpLnRvSGF2ZVVSTCgva2luZD1xdWVzdGlvbiQvKTtcbiAgICBhd2FpdCBwYWdlLmdldEJ5VGVzdElkKFwiaW5ib3gta2luZC1hbGxcIikuY2xpY2soKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9cXC9tXFwvaW5ib3gkLyk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJhcHByb3ZhbC1yb3dcIikpLnRvSGF2ZUNvdW50KDIpO1xuICB9KTtcblxuICB0ZXN0KFwia2luZCByYWRpb2dyb3VwIGlzIGZ1bGx5IDQ0cHggdGFwcGFibGUgYXQgdGhlIGNvYXJzZSBiYW5kIGVkZ2VzXCIsIGFzeW5jICh7IHBhZ2UgfSkgPT4ge1xuICAgIC8vIFVPLTkgcm91bmQtMjogdGhlIHNoYXJlZCBzZWdJdGVtIGlzIDI2cHggVklTSUJMRSBhbmQgaXRzIDQ0cHggcmVhY2ggaXNcbiAgICAvLyB0aGUgOjphZnRlciBiYW5kIDlweCBhYm92ZS9iZWxvdyBpdC4gSW5zaWRlIGFuIG92ZXJmbG93LXggc2Nyb2xscG9ydFxuICAgIC8vIHRoYXQgYmFuZCBtdXN0IHN0YXkgaGl0dGFibGUgKGtlcHQgaW5zaWRlIHRoZSB0cmFjaydzIGJsb2NrIHBhZGRpbmcsXG4gICAgLy8gbm90IGFuIG91dGVyIG1hcmdpbiB0aGUgc2Nyb2xscG9ydCBjbGlwcykuIFRhcCAycHggaW5zaWRlIHRoZSB0b3AgYW5kXG4gICAgLy8gYm90dG9tIG9mIHRoZSA0NHB4IGJhbmQg4oCUIGJvdGggcG9pbnRzIGFyZSBPVVRTSURFIHRoZSAyNnB4IGl0ZW0uXG4gICAgYXdhaXQgcGFnZS5nb3RvKFwiL20vaW5ib3hcIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJtLWluYm94XCIpKS50b0JlVmlzaWJsZSgpO1xuICAgIGNvbnN0IGdyb3VwID0gcGFnZS5nZXRCeVJvbGUoXCJyYWRpb2dyb3VwXCIsIHsgbmFtZTogXCLkuqTkupLnsbvlnotcIiB9KTtcbiAgICBhd2FpdCBleHBlY3QoZ3JvdXApLnRvQmVWaXNpYmxlKCk7XG4gICAgY29uc3QgYmFuZCA9IGF3YWl0IGdyb3VwLmJvdW5kaW5nQm94KCk7XG4gICAgZXhwZWN0KGJhbmQsIFwia2luZCB0cmFjayByZW5kZXJlZFwiKS50b0JlVHJ1dGh5KCk7XG4gICAgZXhwZWN0KGJhbmQhLmhlaWdodCkudG9CZUdyZWF0ZXJUaGFuT3JFcXVhbCg0Myk7XG5cbiAgICBjb25zdCB0ZXN0SWRBdCA9IGFzeW5jICh4OiBudW1iZXIsIHk6IG51bWJlcik6IFByb21pc2U8c3RyaW5nIHwgbnVsbD4gPT5cbiAgICAgIHBhZ2UuZXZhbHVhdGUoXG4gICAgICAgICh7IHgsIHkgfSkgPT4ge1xuICAgICAgICAgIGNvbnN0IGVsID0gZG9jdW1lbnQuZWxlbWVudEZyb21Qb2ludCh4LCB5KSBhcyBIVE1MRWxlbWVudCB8IG51bGw7XG4gICAgICAgICAgcmV0dXJuIGVsID8gKGVsLmRhdGFzZXQudGVzdGlkID8/IGVsLnRhZ05hbWUpIDogbnVsbDtcbiAgICAgICAgfSxcbiAgICAgICAgeyB4LCB5IH0sXG4gICAgICApO1xuXG4gICAgY29uc3QgYXBwcm92YWwgPSBwYWdlLmdldEJ5VGVzdElkKFwiaW5ib3gta2luZC1hcHByb3ZhbFwiKTtcbiAgICBjb25zdCBhYiA9IGF3YWl0IGFwcHJvdmFsLmJvdW5kaW5nQm94KCk7XG4gICAgZXhwZWN0KGFiKS50b0JlVHJ1dGh5KCk7XG4gICAgY29uc3QgY3ggPSBhYiEueCArIGFiIS53aWR0aCAvIDI7XG4gICAgY29uc3QgdG9wWSA9IGJhbmQhLnkgKyAxICsgMjsgLy8gMnB4IGluc2lkZSB0aGUgYmFuZCB0b3AgKGFib3ZlIHRoZSBpdGVtKVxuICAgIGNvbnN0IGJvdHRvbVkgPSBiYW5kIS55ICsgYmFuZCEuaGVpZ2h0IC0gMSAtIDI7IC8vIDJweCBpbnNpZGUgdGhlIGJhbmQgYm90dG9tXG4gICAgLy8gVGhlIDo6YWZ0ZXIgcmVhY2gsIG5vdCB0aGUgMjZweCBnbHlwaCBib3gsIG93bnMgdGhlc2UgcG9pbnRzLlxuICAgIGV4cGVjdChhd2FpdCB0ZXN0SWRBdChjeCwgdG9wWSkpLnRvQmUoXCJpbmJveC1raW5kLWFwcHJvdmFsXCIpO1xuICAgIGV4cGVjdChhd2FpdCB0ZXN0SWRBdChjeCwgYm90dG9tWSkpLnRvQmUoXCJpbmJveC1raW5kLWFwcHJvdmFsXCIpO1xuXG4gICAgLy8gRnVuY3Rpb25hbDogYSB0YXAgb24gdGhlIHVwcGVyIGVkZ2Ugc2VsZWN0cyDlrqHmibk7IG9uIHRoZSBsb3dlciBlZGdlIG9mXG4gICAgLy8g5YWo6YOoIGl0IGNsZWFycyBhZ2Fpbi5cbiAgICBhd2FpdCBwYWdlLm1vdXNlLmNsaWNrKGN4LCB0b3BZKTtcbiAgICBhd2FpdCBleHBlY3QocGFnZSkudG9IYXZlVVJMKC9raW5kPWFwcHJvdmFsJC8pO1xuICAgIGNvbnN0IGFsbCA9IHBhZ2UuZ2V0QnlUZXN0SWQoXCJpbmJveC1raW5kLWFsbFwiKTtcbiAgICBjb25zdCBhbGxCb3ggPSBhd2FpdCBhbGwuYm91bmRpbmdCb3goKTtcbiAgICBleHBlY3QoYWxsQm94KS50b0JlVHJ1dGh5KCk7XG4gICAgYXdhaXQgcGFnZS5tb3VzZS5jbGljayhhbGxCb3ghLnggKyBhbGxCb3ghLndpZHRoIC8gMiwgYm90dG9tWSk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UpLnRvSGF2ZVVSTCgvXFwvbVxcL2luYm94JC8pO1xuICB9KTtcblxuICB0ZXN0KFwiYSBOb2RlLXJlc3RhcnRlZCBzZXNzaW9uIGlzIGZpbHRlcmVkIG91dCBvZiBib3RoIGNvbXBhY3QgaW5ib3ggdGllcnM7IGl0cyBzZXNzaW9uIGJhbm5lciBpcyBuZXV0cmFsXCIsIGFzeW5jICh7XG4gICAgcGFnZSxcbiAgICBicm93c2VyLFxuICB9KSA9PiB7XG4gICAgdGVzdC5zbG93KCk7XG4gICAgLy8gVGhlIG1vYmlsZSBwYWdlIGlzIGxvZ2dlZCBpbiBieSBiZWZvcmVFYWNoIGFuZCBsYW5kcyBvbiAvbSwgd2hvc2UgaG9tZVxuICAgIC8vIGxpc3QgcG9sbHMgc2NyZWVucy9zdW1tYXJpZXMgZm9yIGV2ZXJ5IGluc3RhbmNlIGV2ZXJ5IGZldyBzZWNvbmRzLiBUaGF0XG4gICAgLy8gdHJhZmZpYyBxdWV1ZXMgb24gdGhlIHNoYXJlZCBmYWtlIE5vZGUncyBzaW5nbGUgd2Vic29ja2V0IGFuZCBjYW4gc3RhcnZlXG4gICAgLy8gdGhlIFRUWU5PREVfUkVTVEFSVCBieXRlcywgc28gcGFyayBpdCBvbiBhIGJsYW5rIHBhZ2U6IGNyZWF0ZSwgcmVzdGFydFxuICAgIC8vIGFuZCB2ZXJpZnkgdGhlIHNldHRsZSBlbnRpcmVseSBmcm9tIGEgZGVza3RvcCBwYWdlICh0aGUgZGV0ZXJtaW5pc3RpY1xuICAgIC8vIG5vZGUtaW52ZW50b3J5IG9yZGVyaW5nKSwgYW5kIG1vdW50IHRoZSBpbmJveCBvbmx5IG9uY2UgdGhlIHJvdyBpc1xuICAgIC8vIHNldHRsZWQuXG4gICAgYXdhaXQgcGFnZS5nb3RvKFwiYWJvdXQ6YmxhbmtcIik7XG4gICAgLy8gYnJvd3Nlci5uZXdQYWdlKCkgaW5oZXJpdHMgVEhJUyB0ZXN0J3MgbW9iaWxlIGNvbnRleHQgKGlzTW9iaWxlL3RvdWNoKSxcbiAgICAvLyB1bmRlciB3aGljaCB0aGUgeHRlcm0gaGVscGVyIHN3YWxsb3dzIHJhdyBrZXlib2FyZCBpbnB1dCBhbmQgdGhlXG4gICAgLy8gc2VudGluZWwgbmV2ZXIgcmVhY2hlcyB0aGUgbm9kZS4gVGhlIHJlc3RhcnQgaXMgZHJpdmVuIGZyb20gYSBzZXBhcmF0ZSxcbiAgICAvLyBleHBsaWNpdGx5LWRlc2t0b3AgY29udGV4dCAoY29va2llcyBhcmUgY29waWVkIGJ5IGxvZ2luKSwgbWlycm9yaW5nXG4gICAgLy8gbm9kZS1pbnZlbnRvcnkuaHViLnNwZWMncyBub24tdG91Y2ggcnVuLlxuICAgIGNvbnN0IGRlc2t0b3BDb250ZXh0ID0gYXdhaXQgYnJvd3Nlci5uZXdDb250ZXh0KHtcbiAgICAgIHZpZXdwb3J0OiB7IHdpZHRoOiAxMjgwLCBoZWlnaHQ6IDgwMCB9LFxuICAgICAgaGFzVG91Y2g6IGZhbHNlLFxuICAgICAgaXNNb2JpbGU6IGZhbHNlLFxuICAgIH0pO1xuICAgIGNvbnN0IGRlc2t0b3AgPSBhd2FpdCBkZXNrdG9wQ29udGV4dC5uZXdQYWdlKCk7XG4gICAgYXdhaXQgbG9naW4oZGVza3RvcCwgXCJtLWluYm94LXJlc3RhcnQtZHJpdmVyXCIpO1xuICAgIC8vIFRoZSBIdWIgaWdub3JlcyBhbiBlcG9jaCBoZWxsbyB0aGF0IGNhcnJpZXMgTk8gaW5zdGFuY2UgaW52ZW50b3J5ICh0aGVcbiAgICAvLyAyMDI2LTA5LTE4IGRlbW8gZ3VhcmQ6IG5ldmVyIHdpcGUgcm93cyBvbiBhIHNpbGVudCBub2RlKS4gVGhlIGZpeHR1cmVcbiAgICAvLyBkcm9wcyBvbmx5IHRoZSBzZXNzaW9uIHdob3NlIHR0eSBzZW50IHRoZSBzZW50aW5lbCwgc28gY3JlYXRlIGFcbiAgICAvLyBzdXJ2aXZpbmcgcHR5IHRvbyDigJQgdGhlIGV4YWN0IHNoYXBlIG5vZGUtaW52ZW50b3J5Lmh1Yi5zcGVjIHVzZXMuXG4gICAgY29uc3QgaW5zdGFuY2VJZCA9IGF3YWl0IGNyZWF0ZVJ1bm5pbmdQdHkoZGVza3RvcCk7XG4gICAgY29uc3Qgc3Vydml2b3JJZCA9IGF3YWl0IGNyZWF0ZVJ1bm5pbmdQdHkoZGVza3RvcCk7XG4gICAgYXdhaXQgcmVzdGFydE5vZGVGcm9tVHR5KGRlc2t0b3AsIGluc3RhbmNlSWQpO1xuICAgIGF3YWl0IGV4cGVjdFNldHRsZWRCeVJlc3RhcnQoZGVza3RvcCwgaW5zdGFuY2VJZCk7XG4gICAgYXdhaXQgZGVza3RvcENvbnRleHQuY2xvc2UoKS5jYXRjaCgoKSA9PiB1bmRlZmluZWQpO1xuXG4gICAgLy8gYy1lbmRyZWFzb24gcjI6IHVpLXNwZWMgwqc0LjcgZ2l2ZXMgdGhlIGNvbXBhY3QgaW5ib3ggVFdPIHRpZXJzIGFuZCBub1xuICAgIC8vIHRoaXJkLiBUaGUgZW5kZWQgc2Vzc2lvbiBpcyBmaWx0ZXJlZCBvdXQgZW50aXJlbHk7IHRoZSBsaXZlIHN1cnZpdm9yXG4gICAgLy8gc3RpbGwgYW5jaG9ycyDov5vooYzkuK0gwrcg5pyA6L+RLlxuICAgIGF3YWl0IHBhZ2UuZ290byhcIi9tL2luYm94XCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibS1pbmJveFwiKSkudG9CZVZpc2libGUoKTtcbiAgICBhd2FpdCBleHBlY3QoXG4gICAgICBwYWdlLmxvY2F0b3IoYFtkYXRhLXRlc3RpZD1cIm0taW5ib3gtcmVjZW50LXJvd1wiXVtkYXRhLWluc3RhbmNlLWlkPVwiJHtpbnN0YW5jZUlkfVwiXWApLFxuICAgICkudG9IYXZlQ291bnQoMCk7XG4gICAgLy8gTm8gZW5kZWQgc2VjdGlvbiBleGlzdHMgYXQgYWxsIOKAlCBub3QgbWVyZWx5IGNvbGxhcHNlZC5cbiAgICBhd2FpdCBleHBlY3QocGFnZS5nZXRCeVRlc3RJZChcIm0taW5ib3gtZW5kZWRcIikpLnRvSGF2ZUNvdW50KDApO1xuICAgIGF3YWl0IGV4cGVjdChcbiAgICAgIHBhZ2UubG9jYXRvcihgW2RhdGEtdGVzdGlkPVwibS1pbmJveC1yZWNlbnQtcm93XCJdW2RhdGEtaW5zdGFuY2UtaWQ9XCIke3N1cnZpdm9ySWR9XCJdYCksXG4gICAgKS50b0hhdmVDb3VudCgxLCB7IHRpbWVvdXQ6IDE1XzAwMCB9KTtcbiAgICBleHBlY3QoYXdhaXQgcGFnZS5sb2NhdG9yKFwiW2RhdGEtdGVzdGlkPSdtLWluYm94J11cIikuaW5uZXJUZXh0KCkpLm5vdC50b0NvbnRhaW4oXG4gICAgICBcIm5vZGUtZXBvY2gtY2hhbmdlZFwiLFxuICAgICk7XG5cbiAgICAvLyBUaGUgc2Vzc2lvbiBwYWdlIGtlZXBzIHRoZSByZXN0YXJ0IGJhbm5lcjogbmV1dHJhbCBzZW50ZW5jZSwgbmV1dHJhbFxuICAgIC8vIGNocm9tZSAobmV2ZXIgdGhlIGRhbmdlciBib3JkZXIvYmFja2dyb3VuZCByb2xlKSwgUmVzdW1lIGtlcHQuXG4gICAgYXdhaXQgcGFnZS5nb3RvKGAvcy8ke2luc3RhbmNlSWR9YCk7XG4gICAgY29uc3QgYmFubmVyID0gcGFnZS5nZXRCeVRlc3RJZChcIm5vZGUtcmVzdGFydC1iYW5uZXJcIik7XG4gICAgYXdhaXQgZXhwZWN0KGJhbm5lcikudG9Db250YWluVGV4dChcIk5vZGUg6YeN5ZCv77yM5Lya6K+d5bey5Lit5patXCIsIHsgdGltZW91dDogMjBfMDAwIH0pO1xuICAgIGV4cGVjdChhd2FpdCBiYW5uZXIuaW5uZXJUZXh0KCkpLm5vdC50b0NvbnRhaW4oXCJub2RlLWVwb2NoLWNoYW5nZWRcIik7XG4gICAgYXdhaXQgZXhwZWN0KGJhbm5lcikudG9IYXZlQXR0cmlidXRlKFwidGl0bGVcIiwgXCJub2RlLWVwb2NoLWNoYW5nZWRcIik7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJub2RlLXJlc3RhcnQtcmVzdW1lXCIpKS50b0JlVmlzaWJsZSgpO1xuXG4gICAgY29uc3QgY2hyb21lID0gYXdhaXQgYmFubmVyLmV2YWx1YXRlKChlbCkgPT4ge1xuICAgICAgY29uc3QgcmVzb2x2ZSA9ICh2YWx1ZTogc3RyaW5nLCBwcm9wOiBzdHJpbmcpID0+IHtcbiAgICAgICAgY29uc3QgcHJvYmUgPSBkb2N1bWVudC5jcmVhdGVFbGVtZW50KFwiZGl2XCIpO1xuICAgICAgICBwcm9iZS5zdHlsZS5zZXRQcm9wZXJ0eShwcm9wLCB2YWx1ZSk7XG4gICAgICAgIGRvY3VtZW50LmJvZHkuYXBwZW5kQ2hpbGQocHJvYmUpO1xuICAgICAgICBjb25zdCByZXNvbHZlZCA9IGdldENvbXB1dGVkU3R5bGUocHJvYmUpLmdldFByb3BlcnR5VmFsdWUocHJvcCk7XG4gICAgICAgIHByb2JlLnJlbW92ZSgpO1xuICAgICAgICByZXR1cm4gcmVzb2x2ZWQ7XG4gICAgICB9O1xuICAgICAgY29uc3QgY3MgPSBnZXRDb21wdXRlZFN0eWxlKGVsKTtcbiAgICAgIHJldHVybiB7XG4gICAgICAgIGJvcmRlcjogY3MuZ2V0UHJvcGVydHlWYWx1ZShcImJvcmRlci10b3AtY29sb3JcIiksXG4gICAgICAgIGJhY2tncm91bmQ6IGNzLmdldFByb3BlcnR5VmFsdWUoXCJiYWNrZ3JvdW5kLWNvbG9yXCIpLFxuICAgICAgICBkYW5nZXJCb3JkZXI6IHJlc29sdmUoXCJ2YXIoLS1kYW5nZXItYm9yZGVyKVwiLCBcImJvcmRlci10b3AtY29sb3JcIiksXG4gICAgICAgIGRhbmdlckJnOiByZXNvbHZlKFwidmFyKC0tZGFuZ2VyLWJnKVwiLCBcImJhY2tncm91bmQtY29sb3JcIiksXG4gICAgICAgIG5ldXRyYWxCb3JkZXI6IHJlc29sdmUoXCJ2YXIoLS1ib3JkZXIpXCIsIFwiYm9yZGVyLXRvcC1jb2xvclwiKSxcbiAgICAgICAgbmV1dHJhbEJnOiByZXNvbHZlKFwidmFyKC0tYmctc3VyZmFjZSlcIiwgXCJiYWNrZ3JvdW5kLWNvbG9yXCIpLFxuICAgICAgfTtcbiAgICB9KTtcbiAgICBleHBlY3QoY2hyb21lLmRhbmdlckJvcmRlcikubm90LnRvQmUoXCJcIik7XG4gICAgZXhwZWN0KGNocm9tZS5ib3JkZXIpLm5vdC50b0JlKGNocm9tZS5kYW5nZXJCb3JkZXIpO1xuICAgIGV4cGVjdChjaHJvbWUuYmFja2dyb3VuZCkubm90LnRvQmUoY2hyb21lLmRhbmdlckJnKTtcbiAgICAvLyBJdCBpcyBub3QganVzdCBcImEgZGlmZmVyZW50IHJlZFwiOiB0aGUgYmFubmVyIHVzZXMgdGhlIG5ldXRyYWwgc3VyZmFjZS5cbiAgICBleHBlY3QoY2hyb21lLmJvcmRlcikudG9CZShjaHJvbWUubmV1dHJhbEJvcmRlcik7XG4gICAgZXhwZWN0KGNocm9tZS5iYWNrZ3JvdW5kKS50b0JlKGNocm9tZS5uZXV0cmFsQmcpO1xuXG4gICAgaWYgKGV2aWRlbmNlKSB7XG4gICAgICBjb25zdCBldmlkZW5jZUNvbnRleHQgPSBhd2FpdCBicm93c2VyLm5ld0NvbnRleHQoe1xuICAgICAgICB2aWV3cG9ydDogeyB3aWR0aDogMTQ0MCwgaGVpZ2h0OiA5MDAgfSxcbiAgICAgICAgaGFzVG91Y2g6IGZhbHNlLFxuICAgICAgICBpc01vYmlsZTogZmFsc2UsXG4gICAgICB9KTtcbiAgICAgIGNvbnN0IGV2aWRlbmNlUGFnZSA9IGF3YWl0IGV2aWRlbmNlQ29udGV4dC5uZXdQYWdlKCk7XG4gICAgICBhd2FpdCBsb2dpbihldmlkZW5jZVBhZ2UsIFwibS1pbmJveC1lbmRlZC1ldmlkZW5jZVwiKTtcbiAgICAgIGF3YWl0IGV2aWRlbmNlUGFnZS5nb3RvKGAvcy8ke2luc3RhbmNlSWR9YCk7XG4gICAgICBhd2FpdCBleHBlY3QoZXZpZGVuY2VQYWdlLmdldEJ5VGVzdElkKFwibm9kZS1yZXN0YXJ0LWJhbm5lclwiKSkudG9Db250YWluVGV4dChcbiAgICAgICAgXCJOb2RlIOmHjeWQr++8jOS8muivneW3suS4reaWrVwiLFxuICAgICAgICB7IHRpbWVvdXQ6IDIwXzAwMCB9LFxuICAgICAgKTtcbiAgICAgIGF3YWl0IHNob3QoZXZpZGVuY2VQYWdlLCBcImRlc2t0b3AtZW5kcmVhc29uLW5vZGUtcmVzdGFydC0xNDQwLnBuZ1wiKTtcbiAgICAgIGF3YWl0IGV2aWRlbmNlQ29udGV4dC5jbG9zZSgpO1xuICAgIH1cbiAgfSk7XG5cbiAgdGVzdChcImV2aWRlbmNlOiBpbmJveCBiYW5uZXIgYW5kIGJvdGggdGllcnMgYXQgMzkwcHhcIiwgYXN5bmMgKHsgcGFnZSB9KSA9PiB7XG4gICAgdGVzdC5za2lwKCFldmlkZW5jZSwgXCJzZXQgUkVNVURBX0VWSURFTkNFPTEgdG8gY2FwdHVyZSB0aGUgY29tbWl0dGVkIHNjcmVlbnNob3RcIik7XG4gICAgYXdhaXQgcGFnZS5lbXVsYXRlTWVkaWEoeyByZWR1Y2VkTW90aW9uOiBcInJlZHVjZVwiIH0pO1xuICAgIC8vIE9uZSBhbnN3ZXJlZCBzZXNzaW9uIC0+IGlkbGUgcm93IGluIOi/m+ihjOS4rSDCtyDmnIDov5E7IG9uZSBzdGlsbCBwZW5kaW5nLlxuICAgIGNvbnN0IGFuc3dlcmVkSW5zdGFuY2UgPSBhd2FpdCBjcmVhdGVTZXNzaW9uKHBhZ2UsIFwibSBpbmJveCBldmlkZW5jZSByZWNlbnRcIik7XG4gICAgY29uc3QgYW5zd2VyZWRJbnRlcmFjdGlvbiA9IGF3YWl0IHBlbmRpbmdJbnRlcmFjdGlvbklkKHBhZ2UsIGFuc3dlcmVkSW5zdGFuY2UpO1xuICAgIGF3YWl0IHBhZ2UuZ290byhcIi9tL2luYm94XCIpO1xuICAgIGNvbnN0IGFuc3dlcmVkUm93ID0gaW5ib3hSb3cocGFnZSwgYW5zd2VyZWRJbnRlcmFjdGlvbik7XG4gICAgYXdhaXQgYW5zd2VyZWRSb3cuZ2V0QnlSb2xlKFwiYnV0dG9uXCIsIHsgbmFtZTogXCLlhYHorrjkuIDmrKFcIiB9KS5jbGljaygpO1xuICAgIGF3YWl0IGV4cGVjdChhbnN3ZXJlZFJvdykudG9IYXZlQ291bnQoMCwgeyB0aW1lb3V0OiAyMF8wMDAgfSk7XG4gICAgYXdhaXQgY3JlYXRlU2Vzc2lvbihwYWdlLCBcIm0gaW5ib3ggZXZpZGVuY2UgcGVuZGluZ1wiKTtcblxuICAgIGF3YWl0IHBhZ2UuZ290byhcIi9tL2luYm94XCIpO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibS1pbmJveC1wdXNoLWJhbm5lclwiKSkudG9CZVZpc2libGUoeyB0aW1lb3V0OiAxMF8wMDAgfSk7XG4gICAgYXdhaXQgZXhwZWN0KHBhZ2UuZ2V0QnlUZXN0SWQoXCJhcHByb3ZhbC1yb3dcIikpLnRvSGF2ZUNvdW50KDEsIHsgdGltZW91dDogMTVfMDAwIH0pO1xuICAgIGF3YWl0IGV4cGVjdChwYWdlLmdldEJ5VGVzdElkKFwibS1pbmJveC1yZWNlbnQtcm93XCIpKS50b0hhdmVDb3VudCgxLCB7IHRpbWVvdXQ6IDE1XzAwMCB9KTtcbiAgICBhd2FpdCBzaG90KHBhZ2UsIFwibW9iaWxlLXVpLTQtaW5ib3gtMzkwLnBuZ1wiKTtcbiAgfSk7XG59KTtcbiJdLCJtYXBwaW5ncyI6IkFBQUEsU0FBU0EsTUFBTSxFQUFFQyxJQUFJLFFBQW1CLGtCQUFrQjtBQUMxRCxTQUFTQyxLQUFLLFFBQVEsa0JBQWtCO0FBQ3hDLE9BQU9DLElBQUksTUFBTSxXQUFXO0FBQzVCLFNBQVNDLGFBQWEsUUFBUSxVQUFVO0FBQ3hDLFNBQVNDLEtBQUssUUFBUSxZQUFZOztBQUVsQztBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQTtBQUNBO0FBQ0E7QUFDQUosSUFBSSxDQUFDSyxRQUFRLENBQUNDLFNBQVMsQ0FBQztFQUFFQyxJQUFJLEVBQUU7QUFBUyxDQUFDLENBQUM7QUFFM0MsTUFBTUMsVUFBVSxHQUFHLEdBQUc7QUFFdEIsTUFBTUMsSUFBSSxHQUFHUCxJQUFJLENBQUNRLE9BQU8sQ0FBQ1AsYUFBYSxDQUFDUSxNQUFNLENBQUNDLElBQUksQ0FBQ0MsR0FBRyxDQUFDLENBQUM7QUFDekQsTUFBTUMsUUFBUSxHQUFHQyxPQUFPLENBQUNDLEdBQUcsQ0FBQ0MsZUFBZSxLQUFLLEdBQUc7QUFDcEQsTUFBTUMsT0FBTyxHQUFHSixRQUFRLEdBQ3BCWixJQUFJLENBQUNpQixJQUFJLENBQUNWLElBQUksRUFBRSwrQkFBK0IsQ0FBQyxHQUNoRFAsSUFBSSxDQUFDaUIsSUFBSSxDQUFDVixJQUFJLEVBQUUsNkJBQTZCLENBQUM7QUFFbEQsTUFBTVcsT0FBaUIsR0FBRyxFQUFFO0FBRTVCLGVBQWVDLElBQUlBLENBQUNDLElBQVUsRUFBRUMsSUFBWSxFQUFFO0VBQzVDLE1BQU10QixLQUFLLENBQUNpQixPQUFPLEVBQUU7SUFBRU0sU0FBUyxFQUFFO0VBQUssQ0FBQyxDQUFDO0VBQ3pDLE1BQU1GLElBQUksQ0FBQ0csVUFBVSxDQUFDO0lBQUV2QixJQUFJLEVBQUVBLElBQUksQ0FBQ2lCLElBQUksQ0FBQ0QsT0FBTyxF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