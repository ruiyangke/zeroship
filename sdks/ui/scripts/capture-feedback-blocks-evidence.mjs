// Capture PNG evidence for the Wave 2a feedback blocks (EmptyState,
// ErrorState, Skeleton, Spinner) for visual review.
//
//   STORYBOOK_URL=http://127.0.0.1:PORT node scripts/capture-feedback-blocks-evidence.mjs
//
// Writes PNGs to storybook-static/feedback-evidence/.
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(packageRoot, "storybook-static/feedback-evidence");
// NixOS: bundled Playwright chromium can't link system libs; use the
// system chromium binary (per repo Playwright convention).
const executablePath = process.env.CHROMIUM_PATH || "/run/current-system/sw/bin/chromium";

const captures = [
  { id: "blocks-emptystate--default", file: "emptystate-default" },
  { id: "blocks-emptystate--compound", file: "emptystate-compound" },
  { id: "blocks-errorstate--with-retry", file: "errorstate-retry" },
  { id: "blocks-errorstate--warning-compound", file: "errorstate-warning" },
  { id: "blocks-skeleton--variants", file: "skeleton-variants" },
  { id: "blocks-skeleton--multi-line-text", file: "skeleton-multiline" },
  { id: "blocks-spinner--sizes", file: "spinner-sizes" },
];

const browser = await chromium.launch({ executablePath });
const context = await browser.newContext({
  deviceScaleFactor: 2,
  viewport: { width: 720, height: 520 },
});
const page = await context.newPage();
await mkdir(outDir, { recursive: true });

for (const c of captures) {
  const url = `${baseUrl}/iframe.html?id=${c.id}&viewMode=story`;
  await page.goto(url, { waitUntil: "networkidle" });
  await page.evaluate(() => document.fonts?.ready);
  await page.waitForTimeout(250);
  const out = join(outDir, `${c.file}.png`);
  await page.screenshot({ path: out });
  console.log("captured", out);
}

await browser.close();
