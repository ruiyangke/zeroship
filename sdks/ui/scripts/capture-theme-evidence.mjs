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
  { label: "Studio", value: "studio" },
  { label: "Atelier", value: "atelier" },
  { label: "Dusk", value: "dusk" },
];
const captures = [
  {
    slug: "form",
    storyId: "components-base-ui--field-inputs",
    before: async (page) => {
      await page.locator(".zs-story-form").waitFor({ state: "visible" });
    },
  },
  {
    slug: "open-select",
    storyId: "components-base-ui--field-inputs",
    before: async (page) => {
      await page.locator(".zs-select").first().click();
      await page.locator(".zs-select__popup").waitFor({ state: "visible" });
    },
  },
  {
    slug: "dialog",
    storyId: "components-base-ui--dialog-open",
    before: async (page) => {
      await page.locator(".zs-dialog__panel").waitFor({ state: "visible" });
    },
  },
  {
    slug: "button-states",
    storyId: "components-base-ui--button-states",
    before: async (page) => {
      await page.locator(".zs-button--primary").first().hover();
      await page.locator(".zs-button--secondary").first().focus();
    },
  },
  {
    slug: "choice-controls",
    storyId: "components-base-ui--choice-controls",
    before: async (page) => {
      await page.locator(".zs-switch").waitFor({ state: "visible" });
    },
  },
];

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1100, height: 760 } });
const page = await context.newPage();
const evidence = [];

await mkdir(outDir, { recursive: true });

for (const theme of themes) {
  for (const capture of captures) {
    const url = `${baseUrl}/iframe.html?id=${capture.storyId}&globals=theme:${theme.label}`;
    await page.goto(url, { waitUntil: "networkidle" });
    await page.evaluate(() => document.fonts?.ready);
    await capture.before(page);
    await page.waitForTimeout(250);
    const screenshot = join(outDir, `${theme.value}-${capture.slug}.png`);
    await page.screenshot({ path: screenshot, fullPage: true });
    evidence.push({
      theme: theme.value,
      storyId: capture.storyId,
      kind: capture.slug,
      screenshot,
    });
  }
}

await context.close();
await browser.close();
await writeFile(join(outDir, "theme-evidence.json"), JSON.stringify(evidence, null, 2));
console.log(JSON.stringify(evidence, null, 2));
