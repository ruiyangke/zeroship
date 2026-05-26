import { expect, test } from "@playwright/test";

const HYDRATION_ERROR_RE =
  /Hydration failed|Expected server HTML|server HTML was replaced|There was an error while hydrating/i;

test("serves SSR HTML through Vite and hydrates without replacing the root", async ({ page }) => {
  const failures: string[] = [];

  page.on("console", (message) => {
    const text = message.text();
    if (message.type() === "error" && HYDRATION_ERROR_RE.test(text)) {
      failures.push(text);
    }
  });
  page.on("pageerror", (error) => {
    failures.push(error.message);
  });

  const response = await page.goto("/", { waitUntil: "networkidle" });
  expect(response?.status()).toBe(200);

  await expect(page.getByRole("heading", { name: "ssr-blog" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Why SSR is back in fashion" })).toBeVisible();

  await expect
    .poll(() => page.evaluate(() => Boolean((window as typeof window & { __SSR_PROPS__?: unknown }).__SSR_PROPS__)))
    .toBe(true);

  const clientModule = await page.request.get("/src/entry-client.tsx");
  expect(clientModule.status()).toBe(200);
  expect(clientModule.headers()["content-type"]).toContain("javascript");

  await page.getByRole("link", { name: "Why SSR is back in fashion" }).click();
  await expect(page).toHaveURL(/\/post\/first$/);
  await expect(page.getByRole("heading", { name: "Why SSR is back in fashion" })).toBeVisible();

  await page.waitForTimeout(250);
  expect(failures).toEqual([]);
});
