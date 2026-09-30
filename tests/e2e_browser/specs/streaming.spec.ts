import { test, expect } from "@playwright/test";
import { appUrl } from "../helpers";

// RPC streaming, consumed CLIENT-SIDE in the browser. csr-todo's search box
// drives `searchTodos` (a `stream()` RPC, kind: "stream") via an async iterator
// in App.tsx: each yielded Todo is appended to React state and rendered as a
// new <li>. searchTodos is declared `auth: anonymous, publiclyAccessible: true`,
// so the anonymous browser reaches it through the gateway.
test.describe("RPC streaming (csr-todo searchTodos)", () => {
  // The mount default-"build" stream's data frame renders in the browser DOM:
  // proof that anonymous streaming RPC reaches the browser over the gateway
  // (the transport works end-to-end into client React state).
  test("a streamed match renders in the browser over the gateway", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
    await expect(page.locator("h1")).toHaveText("csr-todo");

    const streamItems = page.locator("section ul li");
    // The default "build" query yields exactly one match; it arrives over the
    // SSE stream and is appended to the rendered list.
    await expect(streamItems).toHaveCount(1);
    await expect(streamItems.nth(0)).toHaveText("Build the CSR demo");
  });

  // Incremental multi-frame rendering after a re-query. The mount "build"
  // stream is the first on the isolate; changing the query starts a second
  // stream on the same isolate, which has to deliver every frame and then end.
  // crates/zeroship-runtime/tests/iss71_sequential_streams.rs pins the runtime
  // half of that without a browser. Asserts the <li>s climb incrementally to 3
  // and the stream TERMINATES (status flips to "3 matches").
  test("typing a query streams matches into the DOM one frame at a time", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
    await expect(page.locator("h1")).toHaveText("csr-todo");

    const streamSection = page.locator("section");
    const streamItems = streamSection.locator("ul li");
    await expect(streamItems).toHaveCount(1);

    const search = page.getByRole("textbox");
    await search.fill("the");

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
      await page.waitForTimeout(8);
    }

    await expect(streamItems).toHaveCount(3);
    await expect(streamItems.nth(0)).toHaveText("Read the .zship spec");
    await expect(streamItems.nth(1)).toHaveText("Build the CSR demo");
    await expect(streamItems.nth(2)).toHaveText("Verify the manifest");

    // The stream TERMINATES: status flips from "streaming…" to "3 matches"
    // once the consumer's iterator reports completion.
    await expect(streamSection.getByText(/^3 matches$/)).toBeVisible();

    const intermediates = [...observed].filter((n) => n > 0 && n < 3);
    expect(
      intermediates.length,
      `expected to observe an intermediate streamed frame; saw counts ${[...observed].sort((a, b) => a - b).join(",")}`,
    ).toBeGreaterThan(0);
  });
});
