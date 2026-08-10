// `describeDemo` — the shape every demo suite shares.
//
// Responsibilities, in the order they matter:
//
//   1. Boot the demo's real dev server (src/dev-server.ts) and launch the
//      chromium this box can run (src/browser.ts).
//   2. If either fails, make EVERY test in the suite fail with that reason
//      attached. Not skipped, not "0 tests" — a demo that cannot be started is
//      a broken demo and has to occupy a red line in the summary.
//   3. Give each test a fresh page, and record what the browser said: console
//      errors, uncaught page errors, and failed network requests. When an
//      assertion fails, that record is attached to the failure, because the
//      cause is nearly always on the browser/server side rather than in the
//      selector.

import { afterAll, afterEach, beforeAll, beforeEach, expect } from "vitest";
import type { ConsoleMessage, Page, Request } from "playwright";
import { closeBrowser, launchBrowser, type BrowserChoice } from "./browser.js";
import { startDemo, type RunningDemo } from "./dev-server.js";
import { demoByName, type Demo } from "./demos.js";

export interface DemoContext {
  readonly demo: Demo;
  /** The page for the current test. Throws if the demo failed to boot. */
  page(): Page;
  /** Origin the browser is pointed at. */
  baseUrl(): string;
  /** Everything the page has complained about so far, formatted. */
  browserDiagnostics(): string;
  /**
   * The subset of the record that means "this app is broken": uncaught page
   * errors and non-2xx RPC responses. Empty array when the app is healthy.
   *
   * Deliberately EXCLUDES console noise and asset 404s. examples/starter ships
   * no favicon, so chromium's implicit `GET /favicon.ico` 404s on every cold
   * load and lands in the console record — intermittently, depending on whether
   * the assertion runs before the request settles. Deciding "does this demo
   * work" on that would make the suite flaky for a cosmetic gap.
   *
   * What this therefore does NOT catch: a missing static asset, a CSP warning,
   * a React key warning, or any console.error the app writes itself. Those stay
   * visible in `browserDiagnostics()` but do not fail a demo on their own.
   */
  appErrors(): string[];
  /**
   * Assert with the browser's console/network record attached to the failure.
   * Use for the assertions whose failure means "the app is broken", so the red
   * carries the actual server error rather than just "expected X, got null".
   */
  expectWithDiagnostics<T>(actual: T): ReturnType<typeof expect<T>>;
}

interface PageRecord {
  consoleErrors: string[];
  pageErrors: string[];
  failedRequests: string[];
  /** Non-2xx responses to the RPC endpoint — the highest-signal line there is. */
  rpcErrors: string[];
}

/**
 * Pull the actual cause out of the dev-server log.
 *
 * The wire only ever carries `{"message":"internal error"}` for a handler
 * throw — deliberately, so app internals do not leak to end users. That is
 * useless to whoever is reading the red, and the real message (the TypeError,
 * the file, the line) is only ever printed by the runtime. So we mine it out
 * and attach it to the failure. Without this the whole tier reports "the todo
 * did not appear" and leaves you to go find out why.
 */
function extractServerErrors(log: string, limit = 6): string[] {
  const out: string[] = [];
  for (const line of log.split("\n")) {
    if (!line.includes('"level":"ERROR"') && !line.includes("dispatch error")) continue;

    const name = /"error\.name":"([^"]*)"/.exec(line)?.[1];
    const message = /"error\.message":"((?:[^"\\]|\\.)*)"/.exec(line)?.[1];
    if (!name && !message) {
      const trimmed = line.trim().slice(0, 220);
      if (trimmed && !out.includes(trimmed)) out.push(trimmed);
      if (out.length >= limit) break;
      continue;
    }

    // The stack is a doubly-escaped blob. Unescape it and keep only the first
    // frame — that is the creator's own file and line, which is what makes the
    // failure actionable.
    const rawStack = /"error\.stack":"([^"]*)"/.exec(line)?.[1] ?? "";
    const frame = /at\s+([^\\\s][^\\]*?:\d+:\d+)\)/.exec(rawStack.replace(/\\+n/g, "\n"))?.[1];

    const rendered = `${name ?? "Error"}: ${message ?? "?"}` + (frame ? `\n      at ${frame}` : "");
    if (!out.includes(rendered)) out.push(rendered);
    if (out.length >= limit) break;
  }
  return out;
}

function formatRecord(rec: PageRecord, serverLog: string): string {
  const sections: string[] = [];
  const add = (title: string, items: string[]) => {
    if (items.length === 0) return;
    sections.push(`${title}:\n${items.map((i) => `    ${i}`).join("\n")}`);
  };
  add("  server-side errors (from the dev runtime log)", extractServerErrors(serverLog));
  add("  RPC errors", rec.rpcErrors);
  add("  uncaught page errors", rec.pageErrors);
  add("  console errors", rec.consoleErrors);
  add("  failed requests", rec.failedRequests);
  if (sections.length === 0) return "  (the browser reported no errors)";
  return sections.join("\n");
}

export function describeDemo(
  demoName: string,
  define: (ctx: DemoContext) => void,
): void {
  const demo = demoByName(demoName);

  let running: RunningDemo | null = null;
  let choice: BrowserChoice | null = null;
  let bootError: Error | null = null;
  let page: Page | null = null;
  let record: PageRecord = {
    consoleErrors: [],
    pageErrors: [],
    failedRequests: [],
    rpcErrors: [],
  };

  beforeAll(async () => {
    try {
      const launched = await launchBrowser();
      choice = launched.choice;
      // eslint-disable-next-line no-console
      console.log(
        `[${demo.name}] browser: ${choice.strategy}` +
          (choice.executablePath ? ` (${choice.executablePath})` : "") +
          ` version=${choice.version}`,
      );
      running = await startDemo(demo);
      // eslint-disable-next-line no-console
      console.log(`[${demo.name}] dev server up: ${running.baseUrl} (api :${demo.apiPort})`);
    } catch (err) {
      // Held, not thrown: throwing here can collapse the suite into a single
      // hook error, and we want the reason on every test line so the summary
      // counts this demo as broken rather than absent.
      bootError = err instanceof Error ? err : new Error(String(err));
    }
  }, 180_000);

  beforeEach(async () => {
    if (bootError) throw bootError;
    const { browser } = await launchBrowser();
    const context = await browser.newContext();
    page = await context.newPage();
    record = { consoleErrors: [], pageErrors: [], failedRequests: [], rpcErrors: [] };

    page.on("console", (msg: ConsoleMessage) => {
      if (msg.type() === "error") record.consoleErrors.push(msg.text());
    });
    page.on("pageerror", (err: Error) => {
      record.pageErrors.push(`${err.name}: ${err.message}`);
    });
    page.on("requestfailed", (req: Request) => {
      record.failedRequests.push(`${req.method()} ${req.url()} — ${req.failure()?.errorText}`);
    });
    page.on("response", async (res) => {
      if (!res.url().includes("/__zeroship/")) return;
      if (res.status() < 400) return;
      let body = "";
      try {
        body = (await res.text()).slice(0, 400);
      } catch {
        body = "(body unavailable)";
      }
      record.rpcErrors.push(`${res.status()} ${res.url()}\n      ${body}`);
    });
  }, 60_000);

  afterEach(async () => {
    const ctx = page?.context();
    page = null;
    await ctx?.close().catch(() => {});
  });

  afterAll(async () => {
    await running?.stop();
    running = null;
    await closeBrowser();
  }, 60_000);

  const context: DemoContext = {
    demo,
    page: () => {
      if (bootError) throw bootError;
      if (!page) throw new Error("no page: called outside a test body?");
      return page;
    },
    baseUrl: () => {
      if (bootError) throw bootError;
      if (!running) throw new Error(`demo ${demo.name} is not running`);
      return running.baseUrl;
    },
    browserDiagnostics: () => formatRecord(record, running?.log() ?? ""),
    appErrors: () => [
      ...extractServerErrors(running?.log() ?? ""),
      ...record.rpcErrors,
      ...record.pageErrors,
    ],
    expectWithDiagnostics: <T,>(actual: T) =>
      expect(actual, `browser diagnostics:\n${formatRecord(record, running?.log() ?? "")}`) as ReturnType<
        typeof expect<T>
      >,
  };

  define(context);
}
