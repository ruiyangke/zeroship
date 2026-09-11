import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";
import { chromium } from "playwright";
import { expect, test } from "vitest";
import { target } from "./target";

test("the built example loads in Chromium and reaches the real runtime", async () => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
    ?? (process.env.PATH ?? "").split(delimiter)
      .flatMap((directory) => ["chromium", "chromium-browser"].map((name) => join(directory, name))).find(existsSync);
  const browser = await chromium.launch({ headless: true, executablePath });
  const page = await browser.newPage();
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await page.goto(target().uiUrl);
    await page.getByText("db-e2e builds a runnable SQLite demo", { exact: false }).waitFor();
    const health = await page.evaluate(async () => {
      const response = await fetch("/__zeroship/v1/db-e2e.health", {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }),
      });
      if (!response.ok) throw new Error(`Health: ${response.status}`);
      return await response.json();
    });
    expect(health).toMatchObject({ json: { ok: true, backend: "sqlite" } });
    expect(errors).toEqual([]);
  } catch (error) {
    await page.screenshot({ path: join(target().logs, "browser.png"), fullPage: true });
    throw error;
  } finally { await browser.close(); }
});
