import { expect, test, type CDPSession, type Page } from "@playwright/test";
import { bootstrapToken, expectCookieSession, logout, useAccessCode } from "./hub-auth";

// Chrome rejects an IP literal as a WebAuthn RP ID ("invalid domain"); the
// loopback RP is `localhost`. Vite is reachable there too and the hub
// allowlists both origins, so drive this spec over localhost (baseURL is
// 127.0.0.1, hence a self-contained login instead of ./hub-auth's login()).
const origin = `http://localhost:${process.env.HUB_E2E_WEB_PORT ?? "58889"}`;

// A passkey round trip is register/start -> CDP virtual-authenticator
// attestation -> register/finish -> GET list -> render, each leg crossing the
// Vite proxy (and the attestation crossing CDP). On a loaded remote browser
// that chain measures several seconds end to end, so the post-ceremony UI
// assertions must wait on the rendered state rather than the 5 s default.
const CEREMONY_UI_TIMEOUT = 20_000;

async function virtualAuthenticator(page: Page): Promise<{ client: CDPSession; id: string }> {
  const client = await page.context().newCDPSession(page);
  await client.send("WebAuthn.enable");
  const { authenticatorId } = await client.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  return { client, id: authenticatorId };
}

async function loginWithCode(page: Page, name: string) {
  await page.goto(`${origin}/login`);
  // Same generous first-render wait as hub-auth's login() under gate load.
  await expect(page.getByTestId("login-page")).toBeVisible({ timeout: 20_000 });
  await useAccessCode(page);
  await page.getByTestId("login-tab-bootstrap").click();
  await page.getByTestId("login-device-name").fill(name);
  await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(new RegExp(`${origin.replace("http://", "http://")}/sessions`));
}

async function expectPasskeySession(page: Page) {
  // Conditional mediation may auto-submit the resident credential; only click
  // if the login page is still present after a short grace period. A session
  // begun on /settings is redirected back to /settings (state.from), so any
  // non-login URL counts; we then navigate to /sessions explicitly.
  const authed = page.waitForURL(/^((?!\/login).)*$/, { timeout: 4000 }).then(
    () => true,
    () => false,
  );
  if (!(await authed)) {
    await page.getByTestId("login-passkey-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0, { timeout: 20_000 });
  }
  await page.goto(`${origin}/sessions`);
  await expect(page.getByTestId("session-list")).toBeVisible({ timeout: CEREMONY_UI_TIMEOUT });
}

test.describe("passkey login (CDP virtual authenticator)", () => {
  test("register in settings, log out, log back in with the passkey", async ({ page }) => {
    const { client, id } = await virtualAuthenticator(page);

    await loginWithCode(page, "passkey-e2e-desk");
    await page.goto(`${origin}/settings`);
    await expect(page.getByTestId("settings-passkeys")).toBeVisible();

    await page.getByTestId("settings-passkey-name").fill("virtual-ctap2");
    await page.getByTestId("settings-passkey-add").click();
    const row = page.getByTestId("settings-passkey-row").filter({ hasText: "virtual-ctap2" });
    await expect(row).toBeVisible({ timeout: CEREMONY_UI_TIMEOUT });
    await expect(row).toContainText("本机");

    // The conditional ceremony the login page fires on mount races the logout
    // cookie clear: a stale remuda_device cookie must never make the Hub
    // answer login/finish 401 before the WebAuthn handler runs (D-030).
    const finishStatuses: number[] = [];
    page.on("response", (response) => {
      if (new URL(response.url()).pathname.endsWith("/v1/auth/passkeys/login/finish")) {
        finishStatuses.push(response.status());
      }
    });

    await logout(page);
    await expect(page).toHaveURL(new RegExp(`${origin}/login`));
    await expect(page.getByTestId("login-page")).toBeVisible();

    await expectPasskeySession(page);
    await expect(page).toHaveURL(/\/sessions/);
    await expectCookieSession(page);
    expect(finishStatuses).toContain(200);
    expect(finishStatuses).not.toContain(401);

    // The new session is a different device; the passkey row still exists.
    await page.goto(`${origin}/settings`);
    await expect(
      page.getByTestId("settings-passkey-row").filter({ hasText: "virtual-ctap2" }),
    ).toBeVisible({ timeout: CEREMONY_UI_TIMEOUT });
    await client.send("WebAuthn.removeVirtualAuthenticator", { authenticatorId: id });
  });

  test("login fails once the authenticator is gone and access code still works", async ({ page }) => {
    const { client, id } = await virtualAuthenticator(page);

    await loginWithCode(page, "passkey-e2e-desk2");
    await page.goto(`${origin}/settings`);
    await page.getByTestId("settings-passkey-name").fill("doomed-key");
    await page.getByTestId("settings-passkey-add").click();
    await expect(
      page.getByTestId("settings-passkey-row").filter({ hasText: "doomed-key" }),
    ).toBeVisible({ timeout: CEREMONY_UI_TIMEOUT });

    // Remove the authenticator: no discoverable credential remains.
    await client.send("WebAuthn.removeVirtualAuthenticator", { authenticatorId: id });

    await logout(page);
    await expect(page).toHaveURL(new RegExp(`${origin}/login`));
    const passkeySubmit = page.getByTestId("login-passkey-submit");
    await passkeySubmit.click();
    // With no authenticator the get() ceremony stays pending (it neither
    // resolves nor rejects): the button holds its busy state and no session is
    // ever established. Wait on that state deterministically instead of a fixed
    // delay, then assert we never left /login.
    await expect(passkeySubmit).toBeDisabled({ timeout: CEREMONY_UI_TIMEOUT });
    await expect(passkeySubmit).toHaveText("等待验证设备…");
    await expect(page.getByTestId("session-list")).toHaveCount(0);
    await expect(page).toHaveURL(new RegExp(`${origin}/login`));

    // Access code remains the fallback. Expanding it aborts the pending get()
    // ceremony, so the bootstrap login is not racing a passkey finish.
    const useCode = page.getByTestId("login-use-code");
    await useCode.click();
    await expect(page.getByTestId("login-codes")).toBeVisible();
    await expect(passkeySubmit).toBeEnabled();
    await page.getByTestId("login-tab-bootstrap").click();
    await page.getByTestId("login-bootstrap-token").fill(bootstrapToken);
    await page.getByTestId("login-device-name").fill("passkey-e2e-fallback");
    await page.getByTestId("login-submit").click();
    await expect(page.getByTestId("login-page")).toHaveCount(0, { timeout: 20_000 });
    await page.goto(`${origin}/sessions`);
    await expect(page.getByTestId("session-list")).toBeVisible({ timeout: CEREMONY_UI_TIMEOUT });
  });
});
