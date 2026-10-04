import { existsSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import { delimiter, join } from "node:path";
import { chromium, type Browser, type Page } from "playwright";
import { logOffset, serverErrors } from "./logs";

/**
 * The Chromium the example suites launch. An explicit
 * PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH wins, then a `chromium` on PATH, which on
 * NixOS is linked against the store and runs there. With neither, Playwright
 * launches the build PLAYWRIGHT_BROWSERS_PATH holds for its own version.
 */
export function chromiumExecutable(): string | undefined {
  return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ?? (process.env.PATH ?? "").split(delimiter)
    .flatMap((directory) => ["chromium", "chromium-browser"].map((name) => join(directory, name))).find(existsSync);
}

/** Launch that Chromium headless, or fail naming the browser it tried and the fix. */
export async function launchChromium(): Promise<Browser> {
  const executablePath = chromiumExecutable();
  try {
    return await chromium.launch({ headless: true, executablePath });
  } catch (error) {
    const { version } = createRequire(import.meta.url)("playwright/package.json") as { version: string };
    throw new Error(executablePath
      ? `playwright ${version} could not launch the Chromium at ${executablePath}, which ` +
        "PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH or else the first chromium on PATH selected."
      : `playwright ${version} could not launch Chromium from PLAYWRIGHT_BROWSERS_PATH=` +
        `${process.env.PLAYWRIGHT_BROWSERS_PATH ?? "(unset)"}, which has to hold the build this Playwright ` +
        "names. A shell entered before a flake.lock change exports the old browsers: re-enter `nix develop`. " +
        "`cargo xtask test playwright-browsers` names both sides and the fix (xtask/tests/playwright/mod.rs).",
    { cause: error });
  }
}

/**
 * What a page reported while a test drove it. Uncaught page errors and
 * answers of 400 or more under `/__zeroship/` are problems: either means the
 * app is broken. Console errors, failed requests, other answers of 400 or more
 * and the server's own error records are the context a failure report carries,
 * and not problems on their own: a browser's implicit favicon request, or a
 * stream the page aborts when it moves on, is not the app failing.
 */
export class PageWatch {
  private readonly found: string[] = [];
  private readonly consoleErrors: string[] = [];
  private readonly failedRequests: string[] = [];
  private readonly refusals: string[] = [];
  private readonly reading = new Set<Promise<void>>();
  private readonly offset: number;

  constructor(page: Page, readonly log?: string) {
    this.offset = log ? logOffset(log) : 0;
    page.on("pageerror", (error) => this.found.push(`uncaught ${error.name}: ${error.message}`));
    page.on("console", (message) => {
      if (message.type() === "error") this.consoleErrors.push(message.text());
    });
    page.on("requestfailed", (request) => {
      this.failedRequests.push(`${request.method()} ${request.url()}: ${request.failure()?.errorText}`);
    });
    page.on("response", (response) => {
      if (response.status() < 400) return;
      const answer = `${response.request().method()} ${response.url()} answered ${response.status()}`;
      if (!new URL(response.url()).pathname.startsWith("/__zeroship/")) {
        this.refusals.push(answer);
        return;
      }
      const read = response.text().catch(() => "(body unavailable)")
        .then((body) => { this.found.push(`${answer}: ${body.slice(0, 400)}`); });
      this.reading.add(read);
      void read.finally(() => this.reading.delete(read));
    });
  }

  /**
   * The problems recorded so far, which the watch then forgets. A test that
   * provokes one on purpose takes it here; the page fixture fails a test that
   * leaves one behind.
   */
  async take(): Promise<string[]> {
    await Promise.all(this.reading);
    return this.found.splice(0);
  }

  /** The server's error records since the watch began. */
  serverErrors(): string[] {
    return this.log ? serverErrors(this.log, this.offset) : [];
  }

  /** Everything recorded, for a failure report. */
  report(): string {
    const sections: string[] = [];
    const add = (title: string, items: string[]) => {
      if (items.length > 0) sections.push(`${title}:\n${items.map((item) => `  ${item}`).join("\n")}`);
    };
    add(`server errors since the page opened (${this.log ?? "no server log"})`, this.serverErrors());
    add("problems", this.found);
    add("other answers of 400 or more", this.refusals);
    add("console errors", this.consoleErrors);
    add("failed requests", this.failedRequests);
    return sections.length > 0 ? sections.join("\n") : "the page and the server reported nothing";
  }
}

/** The part of Vitest's test context the page fixture reads. */
interface FixtureContext { task: { name: string; result?: { state: string } } }

/**
 * Vitest fixtures over one target: a fresh browser context and page per test,
 * and the watch over that page. A test that fails, or that leaves a problem
 * behind, fails with the watch's report and a screenshot in `artifacts`. The
 * fixture functions take the test context as an object pattern because that
 * is how Vitest reads which fixtures a fixture depends on.
 */
export function pageFixtures(browser: () => Browser, target: { name: string; log?: string }, artifacts: string) {
  const watches = new WeakMap<Page, PageWatch>();
  return {
    page: async ({ task }: FixtureContext, use: (page: Page) => Promise<void>) => {
      const context = await browser().newContext();
      try {
        const page = await context.newPage();
        const watch = new PageWatch(page, target.log);
        watches.set(page, watch);
        await use(page);
        const problems = await watch.take();
        if (task.result?.state !== "fail" && problems.length === 0) return;
        await mkdir(artifacts, { recursive: true });
        const screenshot = join(artifacts, `${target.name}-${task.name.replace(/[^a-zA-Z0-9_-]/g, "_")}.png`);
        const shot = await page.screenshot({ path: screenshot, fullPage: true })
          .then(() => `screenshot: ${screenshot}`, (error: unknown) => `no screenshot: ${String(error)}`);
        throw new Error([
          problems.length > 0
            ? `"${task.name}" left problems on ${target.name}:\n${problems.map((problem) => `  ${problem}`).join("\n")}`
            : `"${task.name}" failed on ${target.name}; what the page and the server reported:`,
          watch.report(),
          shot,
        ].join("\n"));
      } finally {
        await context.close();
      }
    },
    watch: async ({ page }: { page: Page }, use: (watch: PageWatch) => Promise<void>) => {
      const watch = watches.get(page);
      if (!watch) throw new Error("the page fixture records no watch for this page");
      await use(watch);
    },
  };
}
