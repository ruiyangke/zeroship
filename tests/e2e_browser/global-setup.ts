import { chromium } from "@playwright/test";
import { rmSync } from "node:fs";
import { writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { constants } from "node:os";
import { DESCRIPTOR } from "./fixture/descriptor";
import { Stack } from "./fixture/stack";

/**
 * Bring the stack up, hand the specs its descriptor, and return the teardown.
 *
 * The browser launches before anything is built, so a PLAYWRIGHT_BROWSERS_PATH
 * that lacks the Chromium build this Playwright names fails the run before the
 * stack exists.
 */
export default async function globalSetup(): Promise<() => Promise<void>> {
  const runner = createRequire(import.meta.url)("@playwright/test/package.json").version as string;
  const browser = await chromium.launch().catch((error: unknown) => {
    throw new Error(
      `@playwright/test ${runner} could not launch Chromium from PLAYWRIGHT_BROWSERS_PATH=` +
      `${process.env.PLAYWRIGHT_BROWSERS_PATH ?? "(unset)"}, which has to hold the Chromium build ` +
      "this Playwright version names. A shell entered before a flake.lock change exports the old " +
      "browsers: re-enter `nix develop`. `cargo xtask test playwright-browsers` names both sides " +
      "and the fix (xtask/tests/playwright/mod.rs).",
      { cause: error },
    );
  });
  console.info(`Browser stack: @playwright/test ${runner} launched Chromium ${browser.version()}`);
  await browser.close();

  const stack = await Stack.create();
  // The services run in process groups of their own, so nothing takes them
  // down with this process: every way out of it has to.
  const abandon = () => {
    stack.abandon();
    rmSync(DESCRIPTOR, { force: true });
  };
  // Playwright's runner handles SIGINT alone. SIGTERM or SIGHUP would end the
  // process without running the teardown this function returns.
  const terminate = (signal: NodeJS.Signals) => {
    abandon();
    process.exit(128 + constants.signals[signal as keyof typeof constants.signals]);
  };
  // An exit that skips the teardown - a forced exit, a crash in the runner -
  // still takes the stack down. Removed once the teardown has.
  process.on("exit", abandon);
  process.on("SIGTERM", terminate);
  process.on("SIGHUP", terminate);
  const shutdown = async () => {
    try {
      rmSync(DESCRIPTOR, { force: true });
      await stack.close();
      process.off("exit", abandon);
    } finally {
      process.off("SIGTERM", terminate);
      process.off("SIGHUP", terminate);
    }
  };

  // During bring-up an interrupt runs nothing of ours: Playwright stops
  // waiting for this function and exits. So SIGINT abandons from the handler
  // until setup returns; after that, Playwright runs the teardown on SIGINT.
  process.on("SIGINT", abandon);
  try {
    await writeFile(DESCRIPTOR, JSON.stringify(await stack.start(), null, 2));
  } catch (error) {
    try { await shutdown(); } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "Browser stack setup and cleanup failed");
    }
    throw error;
  } finally {
    process.off("SIGINT", abandon);
  }

  return async () => {
    const errors: unknown[] = [];
    // A service that died while the specs ran fails the run, even if every
    // spec had already finished.
    try { stack.processes.assertAlive(); } catch (error) { errors.push(error); }
    try { await shutdown(); } catch (error) { errors.push(error); }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, "Browser stack: a service died during the run, and teardown failed");
  };
}
