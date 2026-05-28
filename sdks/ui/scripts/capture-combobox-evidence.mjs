/* Capture-combobox-evidence: mirrors capture-select-evidence. */
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

const themes = [{ label: "Crystal", value: "crystal" }];
const stories = [
  { id: "components-combobox--basic", open: true, trigger: '[data-testid="combobox-basic"] input' },
  { id: "components-combobox--multiple", open: false },
  { id: "components-combobox--all-sizes", open: false },
  { id: "components-combobox--all-variants", open: false },
  { id: "components-combobox--empty", open: false },
  { id: "components-combobox--with-label", open: false },
  { id: "components-combobox--required", open: false },
  { id: "components-combobox--long-list", open: true, trigger: '[data-testid="combobox-long-list"] input' },
  { id: "components-combobox--disabled", open: false },
  { id: "components-combobox--rtl", open: false },
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
  // deviceScaleFactor=1 (not 2x) — Combobox + Autocomplete render
  // significant DOM/portal trees; doubling the device-pixel ratio
  // doubled the per-frame paint cost and chromium-headless ran out of
  // GPU command-buffer resources mid-walk. The PNGs still capture
  // clearly at 1x; if visual review demands 2x for one specific
  // surface, run with `npx playwright screenshot --device-scale-factor=2`
  // against the served storybook directly.
  context = await browser.newContext({
    deviceScaleFactor: 1,
    viewport: { width: 1100, height: 760 },
  });
  const page = await context.newPage();
  const evidence = [];
  await mkdir(outDir, { recursive: true });

  for (const theme of themes) {
    for (const story of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${story.id}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "domcontentloaded" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      await page
        .locator(".zs-combobox-input-group, .zs-combobox-popup")
        .first()
        .waitFor({ state: "visible", timeout: 5000 });
      if (story.open && story.trigger) {
        const t = page.locator(story.trigger).first();
        await t.waitFor({ state: "visible", timeout: 5000 });
        await t.click();
        await page.waitForTimeout(300);
      } else {
        await page.waitForTimeout(200);
      }
      const file = join(outDir, `${theme.value}-combobox-${story.id}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId: story.id, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "combobox-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
