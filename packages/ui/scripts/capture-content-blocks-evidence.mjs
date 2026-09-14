// Capture PNG evidence for the Wave 2c content blocks (StatCard, Banner, DescriptionList).
//   STORYBOOK_URL=http://127.0.0.1:PORT node scripts/capture-content-blocks-evidence.mjs
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(packageRoot, "storybook-static/content-evidence");
const executablePath = process.env.CHROMIUM_PATH || "/run/current-system/sw/bin/chromium";

const captures = [
  { id: "blocks-statcard--up", file: "statcard-up" },
  { id: "blocks-statcard--down", file: "statcard-down" },
  { id: "blocks-statcard--with-icon", file: "statcard-icon" },
  { id: "blocks-banner--intents", file: "banner-intents" },
  { id: "blocks-banner--with-actions", file: "banner-actions" },
  { id: "blocks-descriptionlist--horizontal", file: "dl-horizontal" },
];

const browser = await chromium.launch({ executablePath });
const context = await browser.newContext({ deviceScaleFactor: 2, viewport: { width: 760, height: 520 } });
const page = await context.newPage();
await mkdir(outDir, { recursive: true });
for (const c of captures) {
  await page.goto(`${baseUrl}/iframe.html?id=${c.id}&viewMode=story`, { waitUntil: "networkidle" });
  await page.evaluate(() => document.fonts?.ready);
  await page.waitForTimeout(250);
  await page.screenshot({ path: join(outDir, `${c.file}.png`) });
  console.log("captured", c.file);
}
await browser.close();
