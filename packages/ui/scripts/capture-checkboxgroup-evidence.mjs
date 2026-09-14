/*
 * Capture-checkboxgroup-evidence: mirrors capture-radio-evidence.mjs.
 *
 * Walks each CheckboxGroup story; writes a Crystal-themed PNGs per story.
 * The group itself paints nothing of its own — the screenshots verify
 * the LAYOUT (vertical stack vs horizontal wrap), the child chip paint
 * inherited from Checkbox, and the RTL flip.
 */
import { mkdir, writeFile, stat, readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import { chromium, webkit } from "@playwright/test";

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
  "components-checkboxgroup--basic",
  "components-checkboxgroup--controlled",
  "components-checkboxgroup--disabled",
  "components-checkboxgroup--horizontal",
  "components-checkboxgroup--nested-fieldset",
  "components-checkboxgroup--rtl",
  "components-checkboxgroup--all-selected",
  "components-checkboxgroup--none-selected",
];

async function launchBrowser() {
  try {
    return await chromium.launch();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!/sandbox_host_linux|crashpad/.test(message)) {
      throw error;
    }
    console.warn("Chromium launch failed in this sandbox; falling back to WebKit.");
    return webkit.launch();
  }
}

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
  browser = await launchBrowser();
  // deviceScaleFactor=1 keeps the PNG sizes manageable and matches the
  // Autocomplete capture; chips render crisply at 1x because they use
  // logical SVG paths rather than rasterised glyphs.
  context = await browser.newContext({
    deviceScaleFactor: 1,
    viewport: { width: 1100, height: 760 },
  });
  const page = await context.newPage();
  const evidence = [];
  await mkdir(outDir, { recursive: true });

  for (const theme of themes) {
    for (const storyId of stories) {
      const themeGlobal = encodeURIComponent(theme.label);
      const target = `${baseUrl}/iframe.html?id=${storyId}&globals=theme:${themeGlobal}`;
      await page.goto(target, { waitUntil: "networkidle" });
      await page.evaluate(() => document.fonts && document.fonts.ready);
      await page
        .locator(".zs-checkbox-group")
        .first()
        .waitFor({ state: "visible", timeout: 10000 });
      // Settle any state transitions before the screenshot. Controlled
      // story re-renders on first mount; give it a beat so the live
      // value readout lands.
      await page.waitForTimeout(300);
      const file = join(outDir, `${theme.value}-checkboxgroup-${storyId}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "checkboxgroup-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
