/*
 * Capture-menu-evidence: spins up an http-server over storybook-static,
 * loads each Menu story under both both Crystal themess, and writes a 2x PNG
 * to storybook-static/theme-evidence/.
 *
 * Mirrors capture-popover-evidence.mjs. Each Menu story starts CLOSED
 * with a real Trigger button; the per-story plan lists the
 * data-testid to click before screenshotting. NestedSubmenu opens the
 * outer menu THEN hovers the Submenu trigger to display the nested
 * popup. PlacementSide opens the bottom-side variant (other sides
 * stay closed; the visible popup pins the screenshot frame).
 */
import { mkdir, writeFile, stat, readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
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
  {
    id: "components-menu--basic-items",
    triggers: ['[data-testid="menu-basic-trigger"]'],
  },
  {
    id: "components-menu--with-groups",
    triggers: ['[data-testid="menu-groups-trigger"]'],
  },
  {
    id: "components-menu--with-separator",
    triggers: ['[data-testid="menu-separator-trigger"]'],
  },
  {
    id: "components-menu--with-checkbox-item",
    triggers: ['[data-testid="menu-checkbox-trigger"]'],
  },
  {
    id: "components-menu--with-radio-group",
    triggers: ['[data-testid="menu-radio-trigger"]'],
  },
  {
    id: "components-menu--with-icons",
    triggers: ['[data-testid="menu-icons-trigger"]'],
  },
  {
    id: "components-menu--with-keyboard-shortcuts",
    triggers: ['[data-testid="menu-kbd-trigger"]'],
  },
  {
    id: "components-menu--nested-submenu",
    triggers: [
      { selector: '[data-testid="menu-submenu-trigger"]', action: "click" },
      // Hover the Submenu trigger to surface the nested popup.
      { selector: '[data-testid="menu-submenu-share"]', action: "hover" },
    ],
  },
  {
    id: "components-menu--with-arrow",
    triggers: ['[data-testid="menu-arrow-trigger"]'],
  },
  {
    id: "components-menu--disabled-item",
    triggers: ['[data-testid="menu-disabled-trigger"]'],
  },
  {
    id: "components-menu--placement-side",
    triggers: ['[data-testid="menu-side-bottom-trigger"]'],
  },
  {
    id: "components-menu--rtl",
    triggers: ['[data-testid="menu-rtl-trigger"]'],
  },
  {
    id: "components-menu--with-link-item-as-child",
    triggers: ['[data-testid="menu-link-trigger"]'],
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
    for (const { id: storyId, triggers } of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      for (const step of triggers) {
        const selector = typeof step === "string" ? step : step.selector;
        const action = typeof step === "string" ? "click" : step.action;
        const trigger = page.locator(selector).first();
        await trigger.waitFor({ state: "visible", timeout: 5000 });
        if (action === "hover") {
          await trigger.hover();
        } else {
          await trigger.click();
        }
        await page.waitForTimeout(300);
      }
      if (triggers.length > 0) {
        await page.locator(".zs-menu-popup").last().waitFor({
          state: "visible",
          timeout: 5000,
        });
        await page.waitForTimeout(400);
      }
      const file = join(outDir, `${theme.value}-menu-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "menu-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
