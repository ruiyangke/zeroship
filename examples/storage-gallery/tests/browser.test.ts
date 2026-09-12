import { existsSync, mkdirSync } from "node:fs";
import { delimiter, join } from "node:path";
import { chromium } from "playwright";
import { expect, inject, test } from "vitest";
import { targets } from "./targets";

function executable(): string | undefined {
  return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? (process.env.PATH ?? "").split(delimiter)
    .flatMap((directory) => ["chromium", "chromium-browser"].map((name) => join(directory, name))).find(existsSync);
}

test("the built example loads its browser assets and reaches storage", async () => {
  const browser = await chromium.launch({ headless: true, executablePath: executable() });
  try {
    for (const target of targets()) {
      const page = await browser.newPage();
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      try {
        await page.goto(target.uiUrl);
        await expect.poll(() => page.locator("#app").textContent()).toContain("storage-gallery");
        const result = await page.evaluate(async () => {
          const response = await fetch("/__zeroship/v1/gallery.list", {
            method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }),
          });
          if (!response.ok) throw new Error("Browser RPC failed: " + response.status + ": " + await response.text());
          return response.json();
        });
        expect(result.json).toMatchObject({ entries: expect.any(Array) });
        expect(errors).toEqual([]);
      } catch (error) {
        const artifacts = inject("storageArtifacts");
        mkdirSync(artifacts, { recursive: true });
        await page.screenshot({ path: join(artifacts, target.name + ".png"), fullPage: true });
        throw error;
      } finally { await page.close(); }
    }
  } finally { await browser.close(); }
});
