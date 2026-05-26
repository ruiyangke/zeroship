import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) {
  throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
}

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = process.env.THEME_EVIDENCE_DIR
  ? resolve(process.env.THEME_EVIDENCE_DIR)
  : join(packageRoot, "storybook-static/theme-evidence");
const themes = [
  { label: "Glass Dark", value: "glass-dark" },
  { label: "Glass Light", value: "glass-light" },
];
const captures = [
  {
    storyId: "components-base-ui--field-inputs",
    file: "components-base-ui--field-inputs",
    before: async (page) => {
      await page.locator(".zs-story-form").waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-base-ui--choice-controls",
    file: "components-base-ui--choice-controls",
    before: async (page) => {
      await page.locator(".zs-switch").waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-base-ui--portaled-popup-proof",
    file: "components-base-ui--portaled-popup-proof",
    before: async (page) => {
      await page.locator(".zs-dialog__panel").waitFor({ state: "visible" });
      await page.locator(".zs-select__popup").waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-base-ui--button-states",
    file: "components-base-ui--button-states",
    before: async (page) => {
      await page.locator(".zs-button--primary").first().hover();
      await page.locator(".zs-button--secondary").first().focus();
    },
  },
];

const browser = await chromium.launch();
const context = await browser.newContext({
  deviceScaleFactor: 2,
  viewport: { width: 1100, height: 760 },
});
const page = await context.newPage();
const evidence = [];

await mkdir(outDir, { recursive: true });

for (const theme of themes) {
  for (const capture of captures) {
    const themeGlobal = encodeURIComponent(theme.label);
    const url = `${baseUrl}/iframe.html?id=${capture.storyId}&globals=theme:${themeGlobal}`;
    await page.goto(url, { waitUntil: "networkidle" });
    await page.evaluate(() => document.fonts?.ready);
    await capture.before(page);
    await page.waitForTimeout(250);
    const screenshot = join(outDir, `${theme.value}-${capture.file}.png`);
    await page.screenshot({ path: screenshot, fullPage: true });
    evidence.push({
      theme: theme.value,
      storyId: capture.storyId,
      screenshot,
    });
  }
}

await context.close();
await browser.close();
await writeFile(join(outDir, "theme-evidence.json"), JSON.stringify(evidence, null, 2));
console.log(JSON.stringify(evidence, null, 2));
