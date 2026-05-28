/*
 * Capture-tooltip-evidence: spins up an http-server over storybook-static,
 * loads each Tooltip story under the Crystal theme, and writes a 2x PNG
 * to storybook-static/theme-evidence/.
 *
 * Tooltips open on hover. Each story is wrapped in `<Tooltip.Provider>`
 * (handled inside Tooltip.stories.tsx via the local `Wrap` component),
 * so the open-delay sharing semantics match the real app shape.
 *
 * Capture flow per story:
 *   1. Navigate to the iframe.
 *   2. Hover the trigger via `page.locator(...).hover()`.
 *   3. Wait the configured `waitMs` (default 800 to clear the 600ms
 *      default delay) for the popup to render.
 *   4. Screenshot.
 *
 * Disabled stays closed — no hover, no wait.
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
  {
    id: "components-tooltip--basic",
    trigger: '[data-testid="tooltip-basic-trigger"]',
    waitMs: 900,
  },
  {
    id: "components-tooltip--with-delay",
    trigger: '[data-testid="tooltip-delay-trigger"]',
    waitMs: 500,
  },
  {
    id: "components-tooltip--with-arrow",
    trigger: '[data-testid="tooltip-arrow-trigger"]',
    waitMs: 500,
  },
  // For PlacementSide, hover the 'top' button so the screenshot lands a
  // visible tooltip somewhere readable.
  {
    id: "components-tooltip--placement-side",
    trigger: '[data-testid="tooltip-side-top-trigger"]',
    waitMs: 500,
  },
  {
    id: "components-tooltip--on-focusable",
    trigger: '[data-testid="tooltip-onfocusable-trigger"]',
    waitMs: 500,
    // Use focus rather than hover so the keyboard-focus path is captured.
    interaction: "focus",
  },
  {
    id: "components-tooltip--rich-content",
    trigger: '[data-testid="tooltip-rich-trigger"]',
    waitMs: 500,
  },
  // Disabled — never opens, no interaction.
  { id: "components-tooltip--disabled", trigger: null, waitMs: 0 },
  {
    id: "components-tooltip--rtl",
    trigger: '[data-testid="tooltip-rtl-trigger"]',
    waitMs: 500,
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
    for (const { id: storyId, trigger, waitMs, interaction } of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      if (trigger) {
        const el = page.locator(trigger).first();
        await el.waitFor({ state: "visible", timeout: 5000 });
        if (interaction === "focus") {
          await el.focus();
        } else {
          await el.hover();
        }
        await page.waitForTimeout(waitMs);
        // Wait for at least one tooltip popup to be present (best-effort).
        await page.locator(".zs-tooltip-popup").first().waitFor({
          state: "visible",
          timeout: 5000,
        }).catch(() => {});
      }
      const file = join(outDir, `${theme.value}-tooltip-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "tooltip-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
