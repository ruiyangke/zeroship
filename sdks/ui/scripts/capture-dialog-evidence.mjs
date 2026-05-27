/*
 * Capture-dialog-evidence: spins up an http-server over storybook-static,
 * loads each Dialog story under the Crystal theme, and writes a 2x PNG
 * to storybook-static/theme-evidence/.
 *
 * Self-contained: serves on a random free port, then cleans up the
 * server + browser whether the run succeeds or fails.
 *
 * Dialog stories use `defaultOpen` so the popup is rendered on load —
 * for stories that gate behind a trigger, this script clicks the
 * trigger so the popup screenshot is meaningful.
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

const themes = [{ label: "Crystal", value: "crystal" }];

const stories = [
  { id: "components-dialog--default", openTrigger: false },
  { id: "components-dialog--sizes", openTrigger: true },
  { id: "components-dialog--placement-top", openTrigger: false },
  { id: "components-dialog--backdrop-tints", openTrigger: true },
  { id: "components-dialog--with-form", openTrigger: false },
  { id: "components-dialog--non-dismissible", openTrigger: false },
  { id: "components-dialog--initial-focus", openTrigger: false },
  { id: "components-dialog--nested", openTrigger: false },
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
    for (const { id: storyId, openTrigger } of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      if (openTrigger) {
        const firstTrigger = page.locator(".zs-button").first();
        await firstTrigger.waitFor({ state: "visible", timeout: 5000 });
        await firstTrigger.click();
      }
      // Wait for either the popup or, if no popup was opened, the
      // trigger. Multi-popup stories (sizes, tints) only show one
      // popup at a time post-click.
      await page.locator(".zs-dialog-popup, .zs-button").first().waitFor({
        state: "visible",
        timeout: 5000,
      });
      await page.waitForTimeout(400);
      const file = join(outDir, `${theme.value}-dialog-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "dialog-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
