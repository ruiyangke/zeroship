// Capture PNG evidence for the Wave 2b chip blocks (Badge, Tag).
//   STORYBOOK_URL=http://127.0.0.1:PORT node scripts/capture-chip-blocks-evidence.mjs
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(packageRoot, "storybook-static/chip-evidence");
const executablePath = process.env.CHROMIUM_PATH || "/run/current-system/sw/bin/chromium";

const captures = [
  { id: "blocks-badge--matrix", file: "badge-matrix" },
  { id: "blocks-badge--sizes", file: "badge-sizes" },
  { id: "blocks-tag--default", file: "tag-default" },
  { id: "blocks-tag--removable", file: "tag-removable" },
  { id: "blocks-tag--filter", file: "tag-filter" },
];

const browser = await chromium.launch({ executablePath });
const context = await browser.newContext({
  deviceScaleFactor: 2,
  viewport: { width: 720, height: 460 },
});
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
