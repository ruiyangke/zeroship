/*
 * Capture-navmenu-evidence: spins up an http-server over storybook-static,
 * loads each NavigationMenu story under both both Crystal themess, clicks any
 * Trigger that needs to open, and writes a 2x PNG to
 * storybook-static/theme-evidence/.
 *
 * Mirrors capture-popover-evidence.mjs. Direct-Link stories (Basic) and
 * Disabled need no click — the trigger never opens, or there's no
 * trigger to begin with.
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
  // Basic has only Links; capture the closed strip.
  { id: "components-navigationmenu--basic", triggers: [] },
  {
    id: "components-navigationmenu--with-content",
    triggers: ['[data-testid="navmenu-content-products"]'],
    popup: '[data-testid="navmenu-content-popup"]',
  },
  {
    id: "components-navigationmenu--with-icons",
    triggers: ['[data-testid="navmenu-icons-products"]'],
  },
  {
    id: "components-navigationmenu--with-viewport",
    triggers: ['[data-testid="navmenu-viewport-products"]'],
    popup: '[data-testid="navmenu-viewport-popup"]',
  },
  {
    id: "components-navigationmenu--with-arrow",
    triggers: ['[data-testid="navmenu-arrow-products"]'],
    popup: '[data-testid="navmenu-arrow-popup"]',
  },
  {
    id: "components-navigationmenu--keyboard-nav",
    triggers: ['[data-testid="navmenu-keyboard-products"]'],
  },
  // Disabled: open the first (enabled) trigger so the disabled trigger
  // sits visible next to an open panel.
  {
    id: "components-navigationmenu--disabled",
    triggers: ['[data-testid="navmenu-disabled-products"]'],
  },
  {
    id: "components-navigationmenu--rtl",
    triggers: ['[data-testid="navmenu-rtl-products"]'],
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
    viewport: { width: 1280, height: 800 },
  });
  const page = await context.newPage();
  const evidence = [];
  await mkdir(outDir, { recursive: true });

  for (const theme of themes) {
    for (const story of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${story.id}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      const triggers = story.triggers ?? [];
      for (const sel of triggers) {
        const trigger = page.locator(sel).first();
        await trigger.waitFor({ state: "visible", timeout: 5000 });
        await trigger.click();
        await page.waitForTimeout(250);
      }
      if (story.popup) {
        await page
          .locator(story.popup)
          .last()
          .waitFor({ state: "visible", timeout: 5000 });
        await page.waitForTimeout(350);
      } else if (triggers.length > 0) {
        // Settle hover/open animations even when we didn't pin a popup
        // testid (icon / keyboard / disabled / rtl stories share the
        // Viewport but don't expose the popup's testid).
        await page.waitForTimeout(500);
      }
      const file = join(outDir, `${theme.value}-navmenu-${story.id}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId: story.id, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "navmenu-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
