import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// RPC streaming, consumed CLIENT-SIDE in the browser. csr-todo's search box
// drives `searchTodos` (a `stream()` RPC, kind: "stream") via an async iterator
// in App.tsx: each yielded Todo is appended to React state and rendered as a
// new <li>. The server yields one match at a time with a 30ms gap between
// yields, so a correct client renders INCREMENTAL frames — the rendered match
// count grows over time, it is not a single one-shot append.
//
// We assert the browser truly renders frames incrementally (the count strictly
// increases through intermediate values), not just that the final set is right.
test.describe("RPC streaming (csr-todo searchTodos) — incremental client render", () => {
  test.skip(!appSlug("csr"), "csr-todo not deployed (dist missing)");

  // FIXME(ISS-69): searchTodos is an ANONYMOUS client stream RPC; the gateway
  // fail-closes it with 401 UNAUTHENTICATED (SEC-5 default auth: user), so no
  // frames ever reach the browser. The stream procedure itself is healthy over
  // the worker /dispatch path (the curl harness asserts the AI-SDK frames there).
  // This spec verifies the BROWSER renders the frames incrementally — flip back
  // to `test(` once anon (or test-authed) client RPC streams through the gateway.
  test.fixme("typing a query streams matches into the DOM one frame at a time", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
    await expect(page.locator("h1")).toHaveText("csr-todo");

    // The search box defaults to "build" (1 match). Switch to "the" → 3 matches
    // ("Read the .zship spec", "Build the CSR demo", "Verify the manifest"),
    // each arriving 30ms apart, so we can watch the count climb.
    const search = page.getByRole("textbox");
    await search.fill("");
    await search.fill("the");

    // The streamed-results <ul> is the last list in the stream box. Track its
    // <li> count over time and record every distinct value we observe.
    const streamItems = page.locator("section ul li");

    const observed = new Set<number>();
    const deadline = Date.now() + 15_000;
    let last = -1;
    while (Date.now() < deadline) {
      const n = await streamItems.count();
      if (n !== last) {
        observed.add(n);
        last = n;
      }
      if (n >= 3) break;
      await page.waitForTimeout(10);
    }

    // Final state: 3 matches rendered, with the expected texts.
    await expect(page.getByText(/^3 matches$/)).toBeVisible();
    await expect(streamItems).toHaveCount(3);
    await expect(page.locator("section").getByText("Read the .zship spec")).toBeVisible();
    await expect(page.locator("section").getByText("Build the CSR demo")).toBeVisible();
    await expect(page.locator("section").getByText("Verify the manifest")).toBeVisible();

    // Incrementality: we must have seen at least one intermediate frame (a count
    // strictly between 0 and the final 3) — proof the browser rendered frames as
    // they streamed, not one fused append.
    const intermediates = [...observed].filter((n) => n > 0 && n < 3);
    expect(
      intermediates.length,
      `expected to observe an intermediate streamed frame; saw counts ${[...observed].sort((a, b) => a - b).join(",")}`,
    ).toBeGreaterThan(0);
  });
});
