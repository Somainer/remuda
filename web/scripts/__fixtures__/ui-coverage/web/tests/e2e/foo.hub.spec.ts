import { expect, test } from "@playwright/test";

test("foo and detail", async ({ page }) => {
  await page.goto("/foo");
  const id = "abc";
  await page.goto(`/detail/${id}`);
  await page.goto("/detail/" + id + "?tab=1");
  await expect(page).toHaveURL(/\/m$/);
  // API paths are not views and must not match a route.
  await page.request.get("/v1/hosts");
});
