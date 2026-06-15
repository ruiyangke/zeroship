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
  { label: "Crystal Light", value: "crystal-light" },
  { label: "Crystal Dark", value: "crystal-dark" },
  { label: "Studio Light", value: "studio-light" },
  { label: "Ghibli Light", value: "ghibli-light" },
];
const captures = [
  {
    storyId: "components-card--all-variants",
    file: "card-all-variants",
    before: async (page) => {
      await page.locator(".zs-card").first().waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-button--all-styles",
    file: "button-all-styles",
    before: async (page) => {
      await page.locator(".zs-button").first().waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-input--all-variants",
    file: "input-all-variants",
    before: async (page) => {
      await page.locator(".zs-input").first().waitFor({ state: "visible" });
    },
  },
  {
    storyId: "components-popover--with-title-description",
    file: "popover-with-title-description",
    before: async (page) => {
      await page.locator(".zs-popover-popup").waitFor({ state: "visible" });
    },
  },
  {
    storyId: "layouts-appshell--full-shell",
    file: "appshell-full",
    before: async (page) => {
      await page.locator(".zs-app-shell").waitFor({ state: "visible" });
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
