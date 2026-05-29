// Capture PNG evidence for the Wave 2d layout compositions (AppShell, PageHeader).
//   STORYBOOK_URL=http://127.0.0.1:PORT node scripts/capture-layout-compositions-evidence.mjs
import { mkdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const baseUrl = process.env.STORYBOOK_URL;
if (!baseUrl) throw new Error("Set STORYBOOK_URL to the running static Storybook URL.");
const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(packageRoot, "storybook-static/composition-evidence");
const executablePath = process.env.CHROMIUM_PATH || "/run/current-system/sw/bin/chromium";

const captures = [
  { id: "layouts-appshell--full-shell", file: "appshell-full", w: 1100, h: 680 },
  { id: "layouts-appshell--sidebar-collapsed", file: "appshell-collapsed", w: 1100, h: 680 },
  { id: "layouts-pageheader--full", file: "pageheader-full", w: 900, h: 320 },
];

const browser = await chromium.launch({ executablePath });
await mkdir(outDir, { recursive: true });
for (const c of captures) {
  const context = await browser.newContext({ deviceScaleFactor: 2, viewport: { width: c.w, height: c.h } });
  const page = await context.newPage();
  await page.goto(`${baseUrl}/iframe.html?id=${c.id}&viewMode=story`, { waitUntil: "networkidle" });
  await page.evaluate(() => document.fonts?.ready);
  await page.waitForTimeout(250);
  await page.screenshot({ path: join(outDir, `${c.file}.png`) });
  console.log("captured", c.file);
  await context.close();
}
await browser.close();
