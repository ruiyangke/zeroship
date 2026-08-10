// examples/db-todos — the flagship @zeroship/db demo.
//
// This suite exists because db-todos boots, serves index.html with HTTP 200,
// registers all five plugins, and is nonetheless a shell: every procedure that
// touches `env.db` throws, so the UI never gets past its loading state. A check
// that fetched the page would have called it healthy. Nothing did catch it,
// which is why this tier was written.
//
// The user journey driven here is the one in the README: the page loads a
// shared "public ledger" user, you type a task, press Add, and it appears in
// the list; reload and it is still there because it went to the database.
//
// Every assertion is on what the user sees. The last two look at the error
// channels instead, and they print the runtime's own message — the wire
// deliberately flattens a handler throw to "internal error", which tells a
// reader nothing.

import { describe, expect, it } from "vitest";
import { describeDemo } from "../src/suite.js";
import { appeared, inputEnabled, tryClick, tryFill, UI_TIMEOUT } from "../src/ui.js";

const TASK_INPUT = "Add a task…";
/** Either the populated list or the empty state — both mean the reads finished. */
const SETTLED = "ul.list, div.empty";

describe("db-todos", () => {
  describeDemo("db-todos", (ctx) => {
    it("serves the app shell and renders the React app", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "h1");
      ctx.expectWithDiagnostics(await page.textContent("h1")).toBe("Todos");
    });

    it("loads a user, which enables the task input", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });

      // The input is `disabled={!userId}`: it only becomes usable once
      // users.public has returned a row. Someone who cannot type into the box
      // cannot use this app at all, so this is the first real gate.
      ctx
        .expectWithDiagnostics(await inputEnabled(page, TASK_INPUT))
        .toBe(true);
    });

    it("leaves the loading skeleton and shows a list or an empty state", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });

      // `booting` is true while publicUser or listTodos is pending, and the app
      // renders three grey bars. Either the list or the empty state replacing
      // them is proof the reads completed.
      ctx.expectWithDiagnostics(await appeared(page, SETTLED)).toBe(true);
    });

    it("adds a task through the UI and shows it in the list", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, SETTLED);

      const title = `e2e task ${Date.now()}`;
      const typed = await tryFill(page.getByPlaceholder(TASK_INPUT), title);
      const clicked = await tryClick(page.getByRole("button", { name: "Add" }));
      const listed = await appeared(page, `li:has-text("${title}")`);

      ctx
        .expectWithDiagnostics({ typed, clicked, listed })
        .toEqual({ typed: true, clicked: true, listed: true });
    });

    it("keeps the added task after a full page reload (it reached the database)", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, SETTLED);

      const title = `e2e persisted ${Date.now()}`;
      await tryFill(page.getByPlaceholder(TASK_INPUT), title);
      await tryClick(page.getByRole("button", { name: "Add" }));
      const listedBefore = await appeared(page, `li:has-text("${title}")`);

      await page.reload({ waitUntil: "domcontentloaded" });
      const listedAfterReload = await appeared(page, `li:has-text("${title}")`);

      // Optimistic UI puts the row on screen before the write lands, so only
      // the post-reload read separates "we rendered it" from "we stored it".
      ctx
        .expectWithDiagnostics({ listedBefore, listedAfterReload })
        .toEqual({ listedBefore: true, listedAfterReload: true });
    });

    it("does not surface an error toast to the user", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      // The toast auto-dismisses after 3.4s, so look early and briefly.
      await appeared(page, "div.toast", 6_000);

      ctx.expectWithDiagnostics(await page.locator("div.toast").allTextContents()).toEqual([]);
    });

    it("answers every RPC the UI makes without a server-side throw", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, SETTLED);
      // Let the in-flight RPCs report before reading the record.
      await page.waitForTimeout(Math.min(2_000, UI_TIMEOUT));

      expect(ctx.appErrors(), ctx.browserDiagnostics()).toEqual([]);
    });
  });
});
