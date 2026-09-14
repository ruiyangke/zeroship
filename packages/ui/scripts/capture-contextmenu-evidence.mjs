/*
 * Capture-contextmenu-evidence: spins up an http-server over
 * storybook-static, loads each ContextMenu story under the Crystal
 * theme, and writes a 2x PNG to storybook-static/theme-evidence/.
 *
 * Mirrors capture-menu-evidence.mjs but uses a right-click action on
 * the trigger area instead of a left-click on a button. NestedSubmenu
 * hovers the Move-to row after the popup mounts so the nested popup
 * is captured.
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
    id: "components-contextmenu--basic-right-click-area",
    triggers: [
      {
        selector: '[data-testid="contextmenu-basic-trigger"]',
        action: "rightclick",
      },
    ],
  },
  {
    id: "components-contextmenu--with-checkbox-item",
    triggers: [
      {
        selector: '[data-testid="contextmenu-checkbox-trigger"]',
        action: "rightclick",
      },
    ],
  },
  {
    id: "components-contextmenu--nested-submenu",
    triggers: [
      {
        selector: '[data-testid="contextmenu-submenu-trigger"]',
        action: "rightclick",
      },
      {
        selector: '[data-testid="contextmenu-submenu-move"]',
        action: "hover",
      },
    ],
  },
  {
    id: "components-contextmenu--with-disabled-item",
    triggers: [
      {
        selector: '[data-testid="contextmenu-disabled-trigger"]',
        action: "rightclick",
      },
    ],
  },
  {
    id: "components-contextmenu--custom-anchor",
    triggers: [
      {
        selector: '[data-testid="contextmenu-card-trigger"]',
        action: "rightclick",
      },
    ],
  },
  {
    id: "components-contextmenu--rtl",
    triggers: [
      {
        selector: '[data-testid="contextmenu-rtl-trigger"]',
        action: "rightclick",
      },
    ],
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
        const trigger = page.locator(step.selector).first();
        await trigger.waitFor({ state: "visible", timeout: 5000 });
        if (step.action === "hover") {
          await trigger.hover();
        } else if (step.action === "rightclick") {
          await trigger.click({ button: "right" });
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
      const file = join(outDir, `${theme.value}-contextmenu-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "contextmenu-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
