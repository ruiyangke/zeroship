// Standalone e2e driver — uses Playwright directly (no test runner) so
// it can run from `nix develop` without pulling Playwright into the
// example's node_modules. Drives the dev-server build of ai-chat as a
// real user: open page, type prompt, click Send, watch the streaming
// reply land in the assistant bubble.
//
// Run with:
//   nix develop -c pnpm e2e
// or:
//   pnpm e2e   (if you've already entered the Nix dev shell)
//
// Assumes `pnpm dev` is already serving on http://localhost:5173.

// Locate the `playwright` package from the Nix-provided
// playwright-test bundle so we don't need to add it to node_modules.
// The flake's PLAYWRIGHT_BROWSERS_PATH points at the browsers; the JS
// package sits in a sibling /nix/store/playwright-test-*/lib/node_modules
// path. We `nix-locate`-style probe by spawning `nix shell
// nixpkgs#playwright-test --command which playwright`.
import { readdirSync, existsSync, readlinkSync } from "node:fs";
import { execSync } from "node:child_process";
import { createRequire } from "node:module";

function resolvePlaywrightPath() {
  // 1. Honour explicit PLAYWRIGHT_PATH env if set (override).
  if (process.env.PLAYWRIGHT_PATH) return process.env.PLAYWRIGHT_PATH;

  // 2. Inside the flake's dev shell, the `playwright` CLI is on PATH
  //    and is a symlink into the playwright-test store path. Resolve
  //    it and walk up to /lib/node_modules/playwright/index.mjs.
  try {
    const cli = execSync("which playwright", { encoding: "utf8" }).trim();
    if (cli) {
      // Resolve symlinks: e.g. /run/current-system/sw/bin/playwright
      // → /nix/store/<hash>-playwright-test-*/bin/playwright.
      let real = cli;
      try { real = readlinkSync(cli) || cli; } catch (_) { /* not a symlink */ }
      // Walk up from `bin/playwright` to the package root.
      const storeRoot = real.replace(/\/bin\/playwright.*$/, "");
      const p = `${storeRoot}/lib/node_modules/playwright/index.mjs`;
      if (existsSync(p)) return p;
    }
  } catch (_) { /* fall through */ }

  // 3. Fallback: scan /nix/store for any playwright-test-* package.
  try {
    const candidates = readdirSync("/nix/store").filter((d) =>
      d.startsWith("playwright-test-"),
    );
    for (const d of candidates) {
      const p = `/nix/store/${d}/lib/node_modules/playwright/index.mjs`;
      if (existsSync(p)) return p;
    }
  } catch (_) { /* fall through */ }

  // 4. Last resort: plain Node resolution (works if `playwright` is in
  //    node_modules — handy outside Nix).
  const require = createRequire(import.meta.url);
  return require.resolve("playwright");
}

const playwrightPath = resolvePlaywrightPath();
const { chromium } = await import(playwrightPath);

function resolveHeadlessShell() {
  if (process.env.CHROMIUM_PATH) return process.env.CHROMIUM_PATH;
  // The flake exports PLAYWRIGHT_BROWSERS_PATH with chromium_headless_shell-NNNN.
  const browsers = process.env.PLAYWRIGHT_BROWSERS_PATH;
  if (browsers && existsSync(browsers)) {
    const dir = readdirSync(browsers).find((d) =>
      d.startsWith("chromium_headless_shell-"),
    );
    if (dir) {
      return `${browsers}/${dir}/chrome-headless-shell-linux64/chrome-headless-shell`;
    }
  }
  throw new Error(
    "Couldn't locate chromium headless-shell. Set CHROMIUM_PATH or run inside `nix develop`.",
  );
}

const BASE_URL = process.env.BASE_URL ?? "http://localhost:5173";

async function main() {
  // The Nix-provided playwright bundle and the playwright npm package
  // version-mismatch (`chromium-1194` expected, `chromium-1208` shipped
  // in the Nix browsers path). Pass `executablePath` directly to the
  // bundle's headless-shell so we don't fall down the version check.
  const headlessShell = resolveHeadlessShell();
  const browser = await chromium.launch({
    headless: true,
    executablePath: headlessShell,
  });
  const ctx = await browser.newContext();
  const page = await ctx.newPage();

  // Stream all browser console output back to our stdout — turns
  // useChat's internal logging + any stack traces into actionable
  // signal, not silent failure.
  page.on("console", (msg) =>
    console.log(`[browser ${msg.type()}] ${msg.text()}`),
  );
  page.on("pageerror", (err) =>
    console.error(`[browser pageerror] ${err.message}\n${err.stack}`),
  );
  page.on("requestfailed", (req) =>
    console.error(`[browser requestfail] ${req.url()} - ${req.failure()?.errorText}`),
  );

  let failed = false;
  const fail = (msg) => {
    console.error(`✗ ${msg}`);
    failed = true;
  };

  try {
    console.log(`→ navigating to ${BASE_URL}`);
    await page.goto(BASE_URL, { waitUntil: "domcontentloaded", timeout: 30_000 });

    // 1) Page mounted.
    await page.getByRole("heading", { name: "AI Chat" }).waitFor({ timeout: 10_000 });
    console.log("✓ heading visible");

    // 2) Send a deterministic prompt.
    const prompt = "Reply in exactly five words.";
    const input = page.getByPlaceholder(/Ask anything/i);
    await input.fill(prompt);
    await page.getByRole("button", { name: "Send" }).click();
    console.log(`✓ submitted prompt: ${prompt}`);

    // 3) User bubble appears with our text.
    await page
      .locator("li", { hasText: prompt })
      .waitFor({ timeout: 5_000 });
    console.log("✓ user bubble rendered");

    // 4) Assistant bubble appears (role label === "assistant").
    const assistantBubble = page
      .locator("li")
      .filter({ has: page.locator("div", { hasText: /^assistant$/ }) });
    await assistantBubble.waitFor({ timeout: 25_000 });
    console.log("✓ assistant bubble appeared");

    // 5) Wait until the stream finishes — the input becomes editable
    //    again when useChat flips status: streaming → ready (the
    //    [DONE] frame arrived).
    await page.getByPlaceholder(/Ask anything/i).waitFor({ state: "visible" });
    // The form button is "Stop" while streaming, "Send" when idle.
    await page.getByRole("button", { name: "Send" }).waitFor({ timeout: 60_000 });
    console.log("✓ stream finished (Send button returned)");

    // 6) Final reply is non-trivial.
    const replyText = (await assistantBubble.innerText())
      .replace(/^assistant\s*/i, "")
      .trim();
    if (replyText.length < 5) {
      fail(`assistant reply too short (got: "${replyText}")`);
    } else {
      console.log(`✓ assistant reply (${replyText.length} chars): ${replyText}`);
    }
  } catch (err) {
    fail(`unexpected error: ${err.message ?? err}`);
    try {
      await page.screenshot({ path: "/tmp/aichat-e2e-fail.png", fullPage: true });
      console.error("→ saved screenshot to /tmp/aichat-e2e-fail.png");
    } catch (_e) { /* ignore */ }
  } finally {
    await browser.close();
  }

  process.exit(failed ? 1 : 0);
}

main();
