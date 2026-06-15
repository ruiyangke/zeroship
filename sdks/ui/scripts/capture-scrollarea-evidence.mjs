/*
 * Capture-scrollarea-evidence: spins up an http-server over
 * storybook-static, loads each ScrollArea story under the Crystal
 * theme, and writes a 2x PNG to storybook-static/theme-evidence/.
 *
 * Self-contained: serves on a random free port, then cleans up the
 * server + browser whether the run succeeds or fails.
 *
 * Every ScrollArea story renders inline (no Trigger to click). We
 * still wait for the Root testid to be visible before screenshotting
 * so the overflow observer has settled and the bar reflects its
 * actual visibility policy.
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

// Each story: the testid that proves the Root mounted. For most, we
// just wait for the Root and screenshot; for the hover-only story we
// also hover the Root so the bar reveals before the screenshot.
const stories = [
  {
    id: "components-scrollarea--basic-vertical",
    testid: "scrollarea-basic-vertical",
  },
  {
    id: "components-scrollarea--basic-horizontal",
    testid: "scrollarea-basic-horizontal",
  },
  { id: "components-scrollarea--both", testid: "scrollarea-both" },
  {
    id: "components-scrollarea--always-visible",
    testid: "scrollarea-always-visible",
  },
  {
    id: "components-scrollarea--hover-only",
    testid: "scrollarea-hover-only",
    hover: true,
  },
  { id: "components-scrollarea--long-list", testid: "scrollarea-long-list" },
  {
    id: "components-scrollarea--grid-content",
    testid: "scrollarea-grid-content",
  },
  {
    id: "components-scrollarea--inside-card",
    testid: "scrollarea-inside-card",
  },
  { id: "components-scrollarea--rtl", testid: "scrollarea-rtl" },
  {
    id: "components-scrollarea--keyboard-scroll",
    testid: "scrollarea-keyboard-root",
  },
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
    for (const { id: storyId, testid, hover } of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      const root = page.locator(`[data-testid="${testid}"]`);
      await root.waitFor({ state: "visible", timeout: 5000 });
      // Let Base UI's overflow observer settle.
      await page.waitForTimeout(400);
      if (hover) {
        await root.hover();
        await page.waitForTimeout(350);
      }
      const file = join(outDir, `${theme.value}-scrollarea-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "scrollarea-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
