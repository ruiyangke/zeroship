// examples/csr-todo — the client-side-rendered SPA demo.
//
// It has no database: the todos are a constant array inside the server module,
// exposed through one `query()` and one `stream()`. So "does it work" here means
// three separate paths are alive:
//
//   * the query RPC          — the four server todos reach the page
//   * client state           — "Add local" appends a row without a round trip
//   * the streaming RPC      — searchTodos yields matches over the AI-SDK data
//                              stream protocol, one frame per todo
//
// The stream is the interesting one: it is the only demo in this tier that
// drives a streaming procedure end to end through a browser, and a broken
// stream shows up as a page that renders fine but whose "N matches" counter
// never leaves zero.

import { describe, expect, it } from "vitest";
import { describeDemo } from "../src/suite.js";
import { appeared, tryClick, tryFill } from "../src/ui.js";

/** Seeded server-side in src/server.ts. */
const SERVER_TODOS = [
  "Read the .zship spec",
  "Build the CSR demo",
  "Verify the manifest",
];

describe("csr-todo", () => {
  describeDemo("csr-todo", (ctx) => {
    it("serves the app shell and renders the React app", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "h1");
      ctx.expectWithDiagnostics(await page.textContent("h1")).toBe("csr-todo");
    });

    it("loads the server todos over the query RPC", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");

      const texts = await page.locator("ul li span").allTextContents();
      for (const expected of SERVER_TODOS) {
        ctx.expectWithDiagnostics(texts).toContain(expected);
      }
    });

    it("reports the loaded count instead of staying on 'loading…'", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");

      // The header reads "loading…" until the query resolves, then "N todos".
      const header = await page.locator("main > p").nth(1).textContent();
      ctx.expectWithDiagnostics(header).toMatch(/^\d+ todos$/);
    });

    it("adds a local todo when the button is clicked", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");

      const before = await page.locator("ul").first().locator("li").count();
      const clicked = await tryClick(page.getByRole("button", { name: "Add local" }));
      await page.waitForFunction(
        (n: number) => document.querySelectorAll("ul")[0].querySelectorAll("li").length > n,
        before,
        { timeout: 5_000 },
      ).catch(() => {});
      const after = await page.locator("ul").first().locator("li").count();

      ctx.expectWithDiagnostics({ clicked, grew: after > before }).toEqual({
        clicked: true,
        grew: true,
      });
    });

    it("streams search results over the stream RPC", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");

      // The search box starts at "build", which matches exactly one todo.
      // Retype it so the stream is driven by a real user edit rather than the
      // mount-time effect.
      const search = page.locator("section input");
      await tryFill(search, "the");

      // Three todos contain "the". They arrive as separate frames ~30ms apart,
      // so counting rows is a race: wait for the component's own end-of-stream
      // signal instead — the status flips from "streaming…" to "N matches"
      // only when the iterator is exhausted.
      const status = page.locator("section p").first();
      await page
        .waitForFunction(
          () => /^\d+ matches/.test(document.querySelector("section p")?.textContent ?? ""),
          undefined,
          { timeout: 10_000 },
        )
        .catch(() => {});

      const texts = await page.locator("section ul li span").allTextContents();
      ctx.expectWithDiagnostics(await status.textContent()).toMatch(/^\d+ matches/);
      ctx.expectWithDiagnostics(texts).toContain("Read the .zship spec");
      ctx.expectWithDiagnostics(texts).toContain("Build the CSR demo");
      ctx.expectWithDiagnostics(texts).toContain("Verify the manifest");
    });

    it("shows the server todos again after a full page reload", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");
      await tryClick(page.getByRole("button", { name: "Add local" }));

      await page.reload({ waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");

      // The local todo is client-only and must be gone; the server todos must
      // come back, which only happens if the query RPC works on a cold page.
      const texts = await page.locator("ul li span").allTextContents();
      ctx.expectWithDiagnostics(texts).toContain("Build the CSR demo");
      ctx.expectWithDiagnostics(texts.some((t) => t.startsWith("Local todo"))).toBe(false);
    });

    it("throws nothing server-side and returns no RPC error", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await appeared(page, "ul li");
      await page.waitForTimeout(2_000);

      expect(ctx.appErrors(), ctx.browserDiagnostics()).toEqual([]);
    });
  });
});
