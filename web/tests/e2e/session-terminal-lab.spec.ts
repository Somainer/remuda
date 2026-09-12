import { expect, test } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

const TTY_LAB = "ins_01993ab0-0000-7000-8000-00000000aa01";
const evidence = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../docs/design/evidence");

test.describe("dev-only TTY lab M0-14", () => {
  test("renders recorded ANSI through the 32-byte frame and freezes on disconnect", async ({ page }) => {
    await page.goto(`/s/${TTY_LAB}/tty`);
    await expect(page.locator("[data-tty-lab='1']")).toBeVisible();
    await expect(page.locator("[data-tty-ready='1']")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("Resume this session with");
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("claude --resume 477c322e-9208-49e1-b5d6-8f79df71cf7f");
    await expect(page.getByRole("button", { name: "本地输入" })).toBeVisible();
    await expect(page.getByRole("button", { name: "直连" })).toBeVisible();
    await expect(page.getByTestId("tty-mode-pill")).toHaveText(/raw|keys/);
    await expect(page.getByTestId("tty-keybar")).toBeVisible();
    await expect(page.getByTestId("tty-key-esc")).toBeVisible();
    await expect(page.getByTestId("tty-key-ctrl-c")).toBeVisible();
    await expect(page.getByRole("link", { name: "终端" })).toBeVisible();
    await expect(page.getByRole("link", { name: "结构" })).toBeVisible();
    if (test.info().project.name === "chromium") {
      await page.screenshot({
        path: path.join(evidence, "terminal-1-lab.png"),
        animations: "disabled",
      });
    }

    await page.evaluate(() => {
      const lab = (window as unknown as { __ttyLab?: { disconnect: () => void } }).__ttyLab;
      lab?.disconnect();
    });
    await expect(page.locator("[data-tty-status='reconnecting']")).toBeVisible();
    await expect(page.getByText("reconnecting")).toBeVisible();
    await expect(page.getByTestId("tty-ansi-preview")).toContainText("Resume this session with");
  });

  test("print sessions do not show the terminal tab", async ({ page }) => {
    await page.goto("/sessions");
    await page.getByText("看 TaskManager spill").first().click();
    await expect(page.getByTestId("session-page")).toBeVisible();
    await expect(page.getByRole("link", { name: "终端" })).toHaveCount(0);
  });
});
