/*
 * Capture-toast-evidence: spins up an http-server over storybook-static,
 * loads each Toast story under the Crystal theme, fires the toast via
 * its imperative trigger, and writes a 2x PNG to
 * storybook-static/theme-evidence/.
 *
 * Toasts are imperative — each story renders a button that, when
 * clicked, calls `useToast().toast(...)`. The capture flow:
 *
 *   1. Navigate to the iframe.
 *   2. Click the story's `trigger` testid to emit the toast.
 *   3. Wait for the enter transition (waitMs ≈ 350 covers the 250ms
 *      base motion plus a paint buffer).
 *   4. Screenshot fullPage so the Viewport corner is visible.
 *
 * Stacked + ImperativeUpdate stories fire multiple toasts in sequence
 * via the same / different triggers; their `extraTriggers` array drives
 * the second click.
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
    id: "components-toast--basic",
    trigger: '[data-testid="toast-basic-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--with-description",
    trigger: '[data-testid="toast-description-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--with-action",
    trigger: '[data-testid="toast-action-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--success",
    trigger: '[data-testid="toast-success-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--error-variant",
    trigger: '[data-testid="toast-error-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--warning",
    trigger: '[data-testid="toast-warning-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--info",
    trigger: '[data-testid="toast-info-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--long-duration",
    trigger: '[data-testid="toast-long-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--persistent",
    trigger: '[data-testid="toast-persistent-trigger"]',
    waitMs: 400,
  },
  // ImperativeUpdate — fire start, then finish; the second call updates
  // the live toast in place (same id) so a single Root remains visible.
  {
    id: "components-toast--imperative-update",
    trigger: '[data-testid="toast-update-start"]',
    extraTriggers: [
      { selector: '[data-testid="toast-update-finish"]', preWaitMs: 250 },
    ],
    waitMs: 450,
  },
  // Stacked — single click emits three toasts back-to-back.
  {
    id: "components-toast--stacked",
    trigger: '[data-testid="toast-stacked-trigger"]',
    waitMs: 600,
  },
  {
    id: "components-toast--position-top",
    trigger: '[data-testid="toast-position-top-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--position-bottom",
    trigger: '[data-testid="toast-position-bottom-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--swipe-to-dismiss",
    trigger: '[data-testid="toast-swipe-trigger"]',
    waitMs: 400,
  },
  {
    id: "components-toast--rtl",
    trigger: '[data-testid="toast-rtl-trigger"]',
    waitMs: 400,
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
    for (const story of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${story.id}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      const triggerEl = page.locator(story.trigger).first();
      await triggerEl.waitFor({ state: "visible", timeout: 5000 });
      await triggerEl.click();
      if (Array.isArray(story.extraTriggers)) {
        for (const extra of story.extraTriggers) {
          if (extra.preWaitMs) {
            await page.waitForTimeout(extra.preWaitMs);
          }
          const extraEl = page.locator(extra.selector).first();
          await extraEl.waitFor({ state: "visible", timeout: 5000 });
          await extraEl.click();
        }
      }
      await page.waitForTimeout(story.waitMs);
      // Best-effort wait for at least one toast to be visible.
      await page
        .locator(".zs-toast-root")
        .first()
        .waitFor({ state: "visible", timeout: 5000 })
        .catch(() => {});
      const file = join(outDir, `${theme.value}-toast-${story.id}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId: story.id, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "toast-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
