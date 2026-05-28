/*
 * Capture-menubar-evidence: spins up an http-server over storybook-static,
 * loads each Menubar story under the Crystal theme, clicks the first
 * trigger (or the trigger named in the plan) to open the menu, then
 * screenshots the open state to storybook-static/theme-evidence/.
 *
 * Mirrors capture-popover-evidence.mjs: every menubar story is captured
 * with at least one menu open so the popup paint is visible. Disabled
 * captures the closed state (its disabled trigger never opens).
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
    id: "components-menubar--basic",
    triggers: ['[data-testid="menubar-basic-file"]'],
    popup: '[data-testid="menubar-basic-file-popup"]',
  },
  {
    id: "components-menubar--with-submenus",
    triggers: [
      '[data-testid="menubar-submenus-file"]',
      // open the submenu by hovering the recent trigger.
      '[data-testid="menubar-submenus-recent"]',
    ],
    // The submenu popup is owned by the project Menu.Submenu wrapper
    // and renders as a `.zs-menu-popup--submenu` panel. We don't tag
    // it with a test id (the wrapper doesn't expose one), so wait for
    // the submenu class instead.
    popup: ".zs-menu-popup--submenu",
    hover: true,
  },
  {
    id: "components-menubar--with-checkbox-item",
    triggers: ['[data-testid="menubar-checkbox-view"]'],
    popup: '[data-testid="menubar-checkbox-view-popup"]',
  },
  {
    id: "components-menubar--with-radio-group",
    triggers: ['[data-testid="menubar-radio-theme"]'],
    popup: '[data-testid="menubar-radio-theme-popup"]',
  },
  {
    id: "components-menubar--keyboard-nav",
    triggers: ['[data-testid="menubar-keyboard-file"]'],
    popup: '[data-testid="menubar-keyboard-file-popup"]',
  },
  // Disabled story: open the first NON-disabled trigger so the closed
  // disabled trigger is visible alongside an open menu.
  {
    id: "components-menubar--disabled",
    triggers: ['[data-testid="menubar-disabled-file"]'],
    popup: '[data-testid="menubar-disabled-file-popup"]',
  },
  {
    id: "components-menubar--with-icons",
    triggers: ['[data-testid="menubar-icons-file"]'],
    popup: '[data-testid="menubar-icons-file-popup"]',
  },
  {
    id: "components-menubar--rtl",
    triggers: ['[data-testid="menubar-rtl-file"]'],
    popup: '[data-testid="menubar-rtl-file-popup"]',
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
      const triggers = story.triggers ?? [];
      for (let i = 0; i < triggers.length; i++) {
        const sel = triggers[i];
        const trigger = page.locator(sel).first();
        await trigger.waitFor({ state: "visible", timeout: 5000 });
        // Always click the first trigger; for submenu chains, hover the
        // subsequent triggers (the parent menu must stay open while we
        // hover the submenu trigger). This matches Base UI's submenu
        // behaviour — hover-after-open opens the submenu.
        if (i === 0 || !story.hover) {
          await trigger.click();
        } else {
          await trigger.hover();
        }
        await page.waitForTimeout(250);
      }
      if (story.popup) {
        await page
          .locator(story.popup)
          .last()
          .waitFor({ state: "visible", timeout: 5000 });
        await page.waitForTimeout(300);
      }
      const file = join(outDir, `${theme.value}-menubar-${story.id}.png`);
      await page.screenshot({ path: file, fullPage: true });
      evidence.push({ theme: theme.value, storyId: story.id, screenshot: file });
    }
  }

  await writeFile(
    join(outDir, "menubar-evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (context) await context.close().catch(() => {});
  if (browser) await browser.close().catch(() => {});
  await new Promise((resolve) => server.close(() => resolve()));
}
