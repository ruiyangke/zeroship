/*
 * Capture-popover-evidence: spins up an http-server over storybook-static,
 * loads each Popover story under both both Crystal themess, and writes a 2x PNG
 * to storybook-static/theme-evidence/.
 *
 * Mirrors capture-dialog-evidence.mjs. Each Popover story starts CLOSED
 * with a real Trigger button — the per-story trigger plan lists the
 * data-testid(s) to click before screenshotting.
 *
 * NestedInDialog needs two clicks (dialog trigger first, then the inner
 * popover trigger). Disabled uses no click — the screenshot captures the
 * trigger button in its disabled state.
 */
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { stat, readFile } from "node:fs/promises";
import { chromium } from "@playwright/test";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const staticDir = join(packageRoot, "storybook-static");
const outDir = process.env.THEME_EVIDENCE_DIR
  ? resolve(process.env.THEME_EVIDENCE_DIR)
  : join(staticDir, "theme-evidence");

const themes = [
  { label: "Crystal Light", value: "crystal-light" },
  { label: "Crystal Dark", value: "crystal-dark" },
  { label: "Studio Light", value: "studio-light" },
  { label: "Ghibli Light", value: "ghibli-light" },
];

const stories = [
  { id: "components-popover--basic", triggers: ['[data-testid="popover-basic-trigger"]'] },
  {
    id: "components-popover--with-title-description",
    triggers: ['[data-testid="popover-titledesc-trigger"]'],
  },
  { id: "components-popover--with-arrow", triggers: ['[data-testid="popover-arrow-trigger"]'] },
  {
    id: "components-popover--with-backdrop",
    triggers: ['[data-testid="popover-backdrop-trigger"]'],
  },
  { id: "components-popover--with-close", triggers: ['[data-testid="popover-close-trigger"]'] },
  {
    id: "components-popover--placement-side",
    triggers: ['[data-testid="popover-side-bottom-trigger"]'],
  },
  {
    id: "components-popover--align-start-center-end",
    triggers: ['[data-testid="popover-align-center-trigger"]'],
  },
  {
    id: "components-popover--nested-in-dialog",
    triggers: [
      '[data-testid="popover-nested-dialog-trigger"]',
      '[data-testid="popover-nested-popover-trigger"]',
    ],
  },
  // Disabled trigger never opens — capture the closed state.
  { id: "components-popover--disabled", triggers: [] },
  { id: "components-popover--rtl", triggers: ['[data-testid="popover-rtl-trigger"]'] },
];

const mimeMap = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "application/javascript; charset=utf-8"],
  [".mjs", "application/javascript; charset=utf-8"],
  [".css", "text/css; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".svg", "image/svg+xml"],
  [".png", "image/png"],
  [".jpg", "image/jpeg"],
  [".woff", "font/woff"],
  [".woff2", "font/woff2"],
  [".map", "application/json; charset=utf-8"],
]);

function contentType(pathname) {
  const dot = pathname.lastIndexOf(".");
  if (dot === -1) return "application/octet-stream";
  return mimeMap.get(pathname.slice(dot).toLowerCase()) ?? "application/octet-stream";
}

async function startStaticServer() {
  const server = createServer(async (req, res) => {
    try {
      let pathname = decodeURIComponent(new URL(req.url, "http://x").pathname);
      if (pathname.endsWith("/")) pathname += "index.html";
      const fsPath = join(staticDir, pathname);
      if (!fsPath.startsWith(staticDir)) {
        res.writeHead(403).end();
        return;
      }
      let stats;
      try {
        stats = await stat(fsPath);
      } catch {
        res.writeHead(404).end();
        return;
      }
      if (stats.isDirectory()) {
        const idx = join(fsPath, "index.html");
        const body = await readFile(idx);
        res.writeHead(200, { "content-type": "text/html; charset=utf-8" }).end(body);
        return;
      }
      const body = await readFile(fsPath);
      res.writeHead(200, { "content-type": contentType(fsPath) }).end(body);
    } catch (err) {
      res.writeHead(500).end(String(err));
    }
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Failed to allocate ephemeral port for static server.");
  }
  return { server, url: `http://127.0.0.1:${address.port}` };
}

const { server, url: baseUrl } = await startStaticServer();
let browser;
let context;
try {
  browser = await chromium.launch();
  context = await browser.newContext({
    deviceScaleFactor: 2,
    viewport: { width: 1100, height: 760 },
  });
  const page = await context.newPage();
  const evidence = [];
  await mkdir(outDir, { recursive: true });

  for (const theme of themes) {
    for (const { id: storyId, triggers } of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      for (const selector of triggers) {
        const trigger = page.locator(selector).first();
        await trigger.waitFor({ state: "visible", timeout: 5000 });
        await trigger.click();
        await page.waitForTimeout(300);
      }
      if (triggers.length > 0) {
        // Wait for the last popup to be visible. For nested-in-dialog
        // the inner popover popup is the visible target.
        const popupSelector =
          storyId === "components-popover--nested-in-dialog"
            ? '[data-testid="popover-nested-popover-popup"]'
            : ".zs-popover-popup";
        await page.locator(popupSelector).last().waitFor({
          state: "visible",
          timeout: 5000,
        });
        await page.waitForTimeout(400);
      }
      const file = join(outDir, `${theme.value}-popover-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "popover-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
