import { expect, test } from "@playwright/test";
import { login } from "./hub-auth";

test("gateway Claude PTY: stop and resume delivers the provider and attaches a new TTY", async ({ page }) => {
  test.skip(process.env.HUB_E2E_EXTERNAL === "1", "Needs the in-process fake Node");
  await login(page, "e2e-resume-overlay");
  const headers = { Origin: new URL(page.url()).origin };
  const hosts = await (await page.request.get("/v1/hosts")).json() as {
    items: { hostId: string; label: string }[];
  };
  const host = hosts.items.find((item) => item.label === "e2e-fake-node");
  expect(host).toBeTruthy();
  const hostId = host!.hostId;
  const workspaces = await (await page.request.get(`/v1/hosts/${hostId}/workspaces`)).json() as {
    workspaces: { workspaceId: string; root: string }[];
  };
  const workspace = workspaces.workspaces[0];
  expect(workspace).toBeTruthy();
  const created: string[] = [];
  let profileId: string | undefined;
  try {
    // The credential is a fake fixture. The same Hub registry and host-scoped
    // SecretBroker delivery used by the browser's provider form are exercised.
    const profile = await page.request.post("/v1/providers", {
      headers,
      data: {
        name: "e2e-resume-gateway",
        kind: "gateway",
        baseUrl: `http://${process.env.HUB_E2E_UPSTREAM_LISTEN ?? "127.0.0.1:58881"}/v1`,
        authToken: "fake-resume-gateway-token",
        scope: `host:${hostId}`,
        models: ["e2e/auto"],
        defaultModel: "e2e/auto",
      },
    });
    expect(profile.ok()).toBe(true);
    profileId = (await profile.json()).id;
    expect(profileId).toBeTruthy();
    const create = await page.request.post("/v1/instances", {
      headers,
      data: {
        hostId, workspaceId: workspace.workspaceId, cwd: workspace.root,
        kind: "claude", driver: "claude-pty", delegation: "gateway",
        providerProfileId: profileId, model: "e2e/auto", name: "e2e-resume-overlay",
      },
    });
    expect(create.ok(), await create.text()).toBe(true);
    const parentId = (await create.json()).instance.instanceId as string;
    created.push(parentId);
    const readInstance = async (id: string) => {
      const response = await page.request.get(`/v1/instances/${id}`);
      expect(response.ok()).toBe(true);
      return await response.json();
    };
    await expect.poll(async () => (await readInstance(parentId)).lifecycle, { timeout: 20_000 }).toBe("running");
    const parent = await readInstance(parentId);
    expect(parent.nativeSessionId).toBeTruthy();

    const closed = await page.request.post(`/v1/instances/${parentId}/commands`, {
      headers, data: { operation: "instance.close", payload: {} },
    });
    expect(closed.ok()).toBe(true);
    await expect.poll(async () => (await readInstance(parentId)).lifecycle, { timeout: 20_000 }).toBe("exited");

    const resumed = await page.request.post(`/v1/instances/${parentId}/resume`, {
      headers, data: { mode: "structured" },
    });
    expect(resumed.ok(), await resumed.text()).toBe(true);
    const childId = (await resumed.json()).instance.instanceId as string;
    created.push(childId);
    expect(childId).not.toBe(parentId);
    // The fake Node emits ready and creates its TTY only after receiving both
    // providerOverlay and providerAuthToken on instance.resume. It rejects a
    // missing delivery and refuses to manufacture a TTY during attachment.
    await expect.poll(async () => (await readInstance(childId)).lifecycle, { timeout: 20_000 }).toBe("running");
    const child = await readInstance(childId);
    expect(child.driver).toBe("claude-pty");
    expect(child.resumedFrom).toBe(parentId);
    expect(child.nativeSessionId).toBe(parent.nativeSessionId);
    expect(child.providerProfileId).toBe(profileId);
    await page.goto(`/s/${childId}/tty`);
    await expect(page.locator("[data-tty-lab='1']")).toHaveAttribute("data-tty-status", "live", { timeout: 20_000 });
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("fake-harness terminal");
    await page.locator(".xterm-helper-textarea").first().focus();
    await page.keyboard.type("RESUME_TTY_ATTACHED");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("RESUME_TTY_ATTACHED");
  } finally {
    await page.goto("/sessions");
    for (const id of created.reverse()) {
      await page.request.delete(`/v1/instances/${id}?force=1`, { headers });
    }
    if (profileId) await page.request.delete(`/v1/providers/${profileId}`, { headers });
  }
});
