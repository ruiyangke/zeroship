import { existsSync, mkdirSync } from "node:fs";
import { delimiter, join } from "node:path";
import { chromium } from "playwright";
import { expect, inject, test } from "vitest";
import { targets } from "./targets";

test("a browser can reach the raw workflow app and submit an order", async () => {
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? (process.env.PATH ?? "").split(delimiter)
    .flatMap(directory => ["chromium", "chromium-browser"].map(name => join(directory, name))).find(existsSync);
  const browser = await chromium.launch({ headless: true, executablePath });
  try {
    for (const target of targets()) {
      const page = await browser.newPage();
      try {
        const response = await page.goto(target.uiUrl);
        expect(response?.status()).toBe(200);
        const run = await page.evaluate(async () => {
          const response = await fetch("/orders", { method: "POST", headers: { "content-type": "application/json" },
            body: JSON.stringify({ orderId: crypto.randomUUID(), sku: "browser-hat", quantity: 1 }) });
          if (response.status !== 202) throw new Error(await response.text());
          return response.json();
        });
        expect(run.runId).toMatch(/^run_/);
      } catch (error) {
        const artifacts = inject("workflowArtifacts");
        mkdirSync(artifacts, { recursive: true });
        await page.screenshot({ path: join(artifacts, target.name + ".png"), fullPage: true });
        throw error;
      } finally { await page.close(); }
    }
  } finally { await browser.close(); }
});
