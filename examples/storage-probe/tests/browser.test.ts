import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { launchChromium } from "@zeroship/example-fixtures";
import { expect, inject, test } from "vitest";
import { targets } from "./targets";

test("the built example loads its browser assets and reaches storage", async () => {
  const browser = await launchChromium();
  try {
    for (const target of targets()) {
      const page = await browser.newPage();
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      try {
        await page.goto(target.uiUrl);
        await expect.poll(() => page.locator("#out").textContent()).toContain("storage-probe");
        const result = await page.evaluate(async () => {
          const response = await fetch("/__zeroship/v1/probe.text", {
            method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }),
          });
          if (!response.ok) throw new Error("Browser RPC failed: " + response.status + ": " + await response.text());
          return response.json();
        });
        expect(result.json).toMatchObject({ found: true, textMatches: true });
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
