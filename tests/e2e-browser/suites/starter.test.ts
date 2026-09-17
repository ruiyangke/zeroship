// examples/starter — the scaffold every creator starts from. If this one is
// broken, the documented first five minutes of the product are broken.
//
// What a user does here: type a message, press Add, see it in the list. The
// messages live in an in-memory array inside the server module, so a reload
// re-fetches from the server and must still show it — that round trip is the
// whole point, and it is what distinguishes a working RPC path from a client
// that merely re-rendered its own local state.

import { describe, expect, it } from "vitest";
import { describeDemo } from "../src/suite.js";

describe("starter", () => {
  describeDemo("starter", (ctx) => {
    it("serves the app shell and renders the React app", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await expect(page.getByRole("heading", { name: "zeroship starter" })).toBeDefined();
      // Wait for React to mount, not just for HTML to arrive.
      await page.waitForSelector("h1", { timeout: 15_000 });
      ctx.expectWithDiagnostics(await page.textContent("h1")).toBe("zeroship starter");
    });

    it("loads the seeded messages over RPC", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });

      // getMessages() seeds two rows server-side. If the RPC path is dead the
      // list stays empty and "Loading..." never clears.
      await page.waitForSelector("ul li", { timeout: 20_000 });
      const items = await page.locator("ul li span").allTextContents();
      ctx
        .expectWithDiagnostics(items)
        .toContain("Build locally with an AI coding agent.");
      ctx.expectWithDiagnostics(items.length).toBeGreaterThanOrEqual(2);
    });

    it("adds a message through the UI and shows it in the list", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await page.waitForSelector("ul li", { timeout: 20_000 });

      const message = `e2e message ${Date.now()}`;
      await page.getByPlaceholder("Write a message").fill(message);
      await page.getByRole("button", { name: "Add" }).click();

      // The list is refreshed from the server after the mutation resolves, so
      // seeing the text here means the write actually reached the server.
      await page.waitForSelector(`li:has-text("${message}")`, { timeout: 20_000 });
      ctx
        .expectWithDiagnostics(await page.locator("ul li span").allTextContents())
        .toContain(message);
    });

    it("keeps the added message after a full page reload", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await page.waitForSelector("ul li", { timeout: 20_000 });

      const message = `e2e persisted ${Date.now()}`;
      await page.getByPlaceholder("Write a message").fill(message);
      await page.getByRole("button", { name: "Add" }).click();
      await page.waitForSelector(`li:has-text("${message}")`, { timeout: 20_000 });

      await page.reload({ waitUntil: "domcontentloaded" });
      await page.waitForSelector("ul li", { timeout: 20_000 });

      // After a reload the client state is gone; anything still on screen came
      // back from the server.
      ctx
        .expectWithDiagnostics(await page.locator("ul li span").allTextContents())
        .toContain(message);
    });

    it("throws nothing server-side and returns no RPC error", async () => {
      const page = ctx.page();
      await page.goto(ctx.baseUrl(), { waitUntil: "domcontentloaded" });
      await page.waitForSelector("ul li", { timeout: 20_000 });
      await page.getByPlaceholder("Write a message").fill("diagnostics probe");
      await page.getByRole("button", { name: "Add" }).click();
      await page.waitForSelector('li:has-text("diagnostics probe")', { timeout: 20_000 });

      expect(ctx.appErrors(), ctx.browserDiagnostics()).toEqual([]);
    });
  });
});
