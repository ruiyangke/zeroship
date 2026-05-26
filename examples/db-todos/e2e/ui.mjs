// End-to-end UI tests for the db-todos demo — drives the real browser
// against the dev-server build (Vite SPA + zeroship runtime), exercising
// the full @zeroship/db surface through the actual UI: create, toggle
// done, archive, delete, the demo seeder, and CROSS-TAB realtime (the
// broker SSE feed). A faithful test: real browser, real RPC, real stream.
//
// Run with the dev server already up on http://localhost:5173:
//   pnpm dev                       (in one shell)
//   nix develop -c pnpm e2e:ui     (browser needs the Nix-built chromium)
// or `pnpm e2e:ui` directly if you're already inside `nix develop`.
//
// Uses the Nix-provided Playwright + browsers (no node_modules install) —
// same resolution trick as examples/ai-chat/e2e/run.mjs. The npm-downloaded
// chromium in ~/.cache lacks system libs (libglib etc.); the Nix browsers
// at $PLAYWRIGHT_BROWSERS_PATH (set by `nix develop`) have them, so we
// prefer that path in resolveHeadlessShell().

import { readdirSync, existsSync, readlinkSync } from "node:fs";
import { execSync } from "node:child_process";
import { createRequire } from "node:module";

function resolvePlaywrightPath() {
  if (process.env.PLAYWRIGHT_PATH) return process.env.PLAYWRIGHT_PATH;
  try {
    const cli = execSync("which playwright", { encoding: "utf8" }).trim();
    if (cli) {
      let real = cli;
      try { real = readlinkSync(cli) || cli; } catch { /* not a symlink */ }
      const storeRoot = real.replace(/\/bin\/playwright.*$/, "");
      const p = `${storeRoot}/lib/node_modules/playwright/index.mjs`;
      if (existsSync(p)) return p;
    }
  } catch { /* fall through */ }
  try {
    const candidates = readdirSync("/nix/store").filter((d) => d.startsWith("playwright-test-"));
    for (const d of candidates) {
      const p = `/nix/store/${d}/lib/node_modules/playwright/index.mjs`;
      if (existsSync(p)) return p;
    }
  } catch { /* fall through */ }
  return createRequire(import.meta.url).resolve("playwright");
}

function resolveHeadlessShell() {
  if (process.env.CHROMIUM_PATH) return process.env.CHROMIUM_PATH;
  const roots = [process.env.PLAYWRIGHT_BROWSERS_PATH, `${process.env.HOME}/.cache/ms-playwright`].filter(Boolean);
  for (const root of roots) {
    if (!existsSync(root)) continue;
    const dir = readdirSync(root).find((d) => d.startsWith("chromium_headless_shell-"));
    if (dir) {
      const p = `${root}/${dir}/chrome-headless-shell-linux64/chrome-headless-shell`;
      if (existsSync(p)) return p;
    }
  }
  throw new Error("Couldn't locate chromium headless-shell. Set CHROMIUM_PATH or PLAYWRIGHT_BROWSERS_PATH.");
}

const BASE_URL = process.env.BASE_URL ?? "http://localhost:5173";
const { chromium } = await import(resolvePlaywrightPath());

let failures = 0;
const pass = (m) => console.log(`  ✓ ${m}`);
const fail = (m) => { console.error(`  ✗ ${m}`); failures++; };
async function check(name, fn) {
  try { await fn(); pass(name); } catch (e) { fail(`${name} — ${e.message ?? e}`); }
}
const uniq = (p) => `${p}-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 5)}`;

// Read the "N open" number from the header count chip.
async function openCount(page) {
  const txt = await page.locator(".count").innerText();
  const m = txt.match(/(\d+)/);
  return m ? Number(m[1]) : NaN;
}

async function bootedPage(ctx) {
  const page = await ctx.newPage();
  page.on("pageerror", (err) => console.error(`[browser pageerror] ${err.message}`));
  await page.goto(BASE_URL, { waitUntil: "domcontentloaded", timeout: 30_000 });
  await page.getByRole("heading", { name: "Todos" }).waitFor({ timeout: 15_000 });
  // The composer input is disabled until the shared "public" user is provisioned.
  await page.getByPlaceholder("Add a task…").waitFor({ state: "visible", timeout: 15_000 });
  await page.waitForFunction(() => {
    const i = document.querySelector("input");
    return i && !i.disabled;
  }, { timeout: 15_000 });
  return page;
}

// Create a todo via the composer; returns the unique title used.
async function addTodo(page, { priority = "high" } = {}) {
  const title = uniq("e2e");
  await page.getByPlaceholder("Add a task…").fill(title);
  await page.locator(`.prio button[aria-label="${priority} priority"]`).click();
  await page.locator("button.add").click();
  await page.locator(".item", { hasText: title }).first().waitFor({ timeout: 10_000 });
  return title;
}

async function main() {
  const browser = await chromium.launch({ headless: true, executablePath: resolveHeadlessShell() });
  const ctx = await browser.newContext();
  try {
    console.log(`→ ${BASE_URL}`);
    const page = await bootedPage(ctx);

    await check("app mounts (heading + enabled composer)", async () => {
      await page.getByRole("heading", { name: "Todos" }).waitFor();
    });

    await check("LIVE indicator turns on (SSE connected)", async () => {
      await page.locator(".live.on").waitFor({ timeout: 15_000 });
    });

    let created;
    await check("create: composer adds a todo that renders", async () => {
      created = await addTodo(page, { priority: "high" });
      await page.locator(".item", { hasText: created }).first().waitFor();
    });

    await check("toggle done: clicking the checkbox marks it done", async () => {
      const item = page.locator(".item", { hasText: created }).first();
      await item.locator(".box").click();
      await page.locator(".item.done", { hasText: created }).first().waitFor({ timeout: 10_000 });
    });

    await check("toggle back: clicking again un-marks it", async () => {
      const item = page.locator(".item", { hasText: created }).first();
      await item.locator(".box").click();
      await page.waitForFunction(
        (t) => ![...document.querySelectorAll(".item.done")].some((el) => el.textContent.includes(t)),
        created,
        { timeout: 10_000 },
      );
    });

    await check("archive: removes the row from the list", async () => {
      const item = page.locator(".item", { hasText: created }).first();
      await item.hover();
      await item.locator('.icon-btn[aria-label="archive"]').click();
      await page.locator(".item", { hasText: created }).first().waitFor({ state: "detached", timeout: 10_000 });
    });

    await check("delete: soft-deletes and removes the row", async () => {
      const t = await addTodo(page);
      const item = page.locator(".item", { hasText: t }).first();
      await item.hover();
      await item.locator('.icon-btn[aria-label="delete"]').click();
      await page.locator(".item", { hasText: t }).first().waitFor({ state: "detached", timeout: 10_000 });
    });

    await check("demo seeder: '+10' button bulk-creates ~10 todos", async () => {
      const before = await openCount(page);
      await page.locator("button.ghost").click();
      // open-count climbs by ~10 (each lands ~500ms apart, optimistic).
      await page.waitForFunction(
        (b) => {
          const m = document.querySelector(".count")?.textContent?.match(/(\d+)/);
          return m && Number(m[1]) >= b + 10;
        },
        before,
        { timeout: 20_000 },
      );
    });

    // ── The headline: cross-tab realtime over the broker SSE feed ──
    await check("realtime: a todo created in tab A appears in tab B", async () => {
      const pageB = await bootedPage(ctx);
      const title = uniq("rt");
      await page.getByPlaceholder("Add a task…").fill(title);
      await page.locator("button.add").click();
      // tab B never created it — it must arrive via the live feed.
      await pageB.locator(".item", { hasText: title }).first().waitFor({ timeout: 15_000 });
      await pageB.close();
    });
  } catch (e) {
    fail(`unexpected error: ${e.message ?? e}`);
    try { await ctx.pages()[0]?.screenshot({ path: "/tmp/db-todos-e2e-fail.png", fullPage: true }); } catch { /* ignore */ }
  } finally {
    await browser.close();
  }

  console.log(failures === 0 ? "\ne2e:ui — all checks passed" : `\ne2e:ui — ${failures} check(s) failed`);
  process.exit(failures ? 1 : 0);
}

main();
