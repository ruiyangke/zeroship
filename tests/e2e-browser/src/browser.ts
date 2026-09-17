// Browser resolution — the NixOS problem, and why this file exists.
//
// Playwright's own downloaded browsers are dynamically linked against a
// standard FHS layout (/lib64/ld-linux-x86-64.so.2, a system libnss, ...).
// NixOS has none of that, so a browser fetched by `playwright install`
// cannot start here at all. Measured on this box with PLAYWRIGHT_BROWSERS_PATH
// unset:
//
//   Error: browserType.launch: Executable doesn't exist at
//   ~/.cache/ms-playwright/chromium_headless_shell-1208/.../chrome-headless-shell
//
// Two things DO work:
//
//   1. The SYSTEM chromium (`/run/current-system/sw/bin/chromium`), which Nix
//      built and patchelf'd against the store. Works unconditionally.
//   2. The Nix `playwright-browsers` derivation, but ONLY when the shell has
//      PLAYWRIGHT_BROWSERS_PATH pointing at it (i.e. inside `nix develop`).
//
// So the order below is: system chromium first (no ambient env required),
// Nix bundle second (works in `nix develop`), and if neither launches we throw
// with EVERY attempt's error attached. We never silently degrade to "no
// browser" — a suite that cannot open a browser must be a red, not a skip.

import { chromium, type Browser, type BrowserType } from "playwright";
import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";

/** How the browser was obtained, for the run banner. */
export interface BrowserChoice {
  strategy: "system-executable-path" | "playwright-browsers-path";
  executablePath?: string;
  version: string;
}

const SYSTEM_CANDIDATES = [
  // Explicit override always wins.
  process.env.ZEROSHIP_E2E_CHROMIUM,
  "chromium",
  "chromium-browser",
  "google-chrome-stable",
  "google-chrome",
] as const;

/** Resolve a candidate to an absolute executable, or undefined. */
function resolveExecutable(candidate: string | undefined): string | undefined {
  if (!candidate) return undefined;
  if (candidate.includes("/")) return existsSync(candidate) ? candidate : undefined;
  try {
    const found = execFileSync("command", ["-v", candidate], {
      encoding: "utf8",
      shell: "/bin/sh",
    }).trim();
    return found.length > 0 && existsSync(found) ? found : undefined;
  } catch {
    return undefined;
  }
}

interface Attempt {
  label: string;
  launch: () => Promise<Browser>;
}

function buildAttempts(browserType: BrowserType, headless: boolean): Attempt[] {
  const attempts: Attempt[] = [];

  for (const candidate of SYSTEM_CANDIDATES) {
    const executablePath = resolveExecutable(candidate);
    if (!executablePath) continue;
    // Deduplicate: several names commonly symlink to the same binary.
    if (attempts.some((a) => a.label.endsWith(executablePath))) continue;
    attempts.push({
      label: `system chromium at ${executablePath}`,
      launch: () =>
        browserType.launch({
          headless,
          executablePath,
          // --no-sandbox: the Chromium sandbox needs user namespaces that are
          // not always available in a container/CI shell. We only ever load
          // localhost pages we built ourselves.
          args: ["--no-sandbox", "--disable-dev-shm-usage"],
        }),
    });
  }

  if (process.env.PLAYWRIGHT_BROWSERS_PATH) {
    attempts.push({
      label: `playwright browsers bundle at ${process.env.PLAYWRIGHT_BROWSERS_PATH}`,
      launch: () =>
        browserType.launch({
          headless,
          args: ["--no-sandbox", "--disable-dev-shm-usage"],
        }),
    });
  }

  return attempts;
}

let cached: { browser: Browser; choice: BrowserChoice } | null = null;

/**
 * Launch (once per process) the chromium this box can actually run.
 *
 * Throws with every attempted strategy and its error if none launched. There is
 * deliberately no "return null" path: a missing browser is a failure of the
 * whole tier, and must read as one.
 */
export async function launchBrowser(): Promise<{ browser: Browser; choice: BrowserChoice }> {
  if (cached) return cached;

  const headless = process.env.ZEROSHIP_E2E_HEADED !== "1";
  const attempts = buildAttempts(chromium, headless);

  if (attempts.length === 0) {
    throw new Error(
      "no chromium available: none of " +
        SYSTEM_CANDIDATES.filter(Boolean).join(", ") +
        " is on PATH and PLAYWRIGHT_BROWSERS_PATH is unset.\n" +
        "Install a system chromium, or run `pnpm exec playwright install chromium` " +
        "so PLAYWRIGHT_BROWSERS_PATH points at a bundle. On NixOS prefer the " +
        "system chromium or `nix develop`: browsers from `playwright install` are " +
        "linked against a standard FHS layout and do not run there.",
    );
  }

  const failures: string[] = [];
  for (const attempt of attempts) {
    try {
      const browser = await attempt.launch();
      const choice: BrowserChoice = {
        strategy: attempt.label.startsWith("system")
          ? "system-executable-path"
          : "playwright-browsers-path",
        executablePath: attempt.label.startsWith("system")
          ? attempt.label.replace("system chromium at ", "")
          : undefined,
        version: browser.version(),
      };
      cached = { browser, choice };
      return cached;
    } catch (err) {
      failures.push(`  - ${attempt.label}\n      ${String(err).split("\n")[0]}`);
    }
  }

  throw new Error(
    `every chromium launch strategy failed (${attempts.length} tried):\n${failures.join("\n")}`,
  );
}

export async function closeBrowser(): Promise<void> {
  if (!cached) return;
  const { browser } = cached;
  cached = null;
  await browser.close().catch(() => {});
}
