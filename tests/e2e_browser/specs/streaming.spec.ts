import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// RPC streaming, consumed CLIENT-SIDE in the browser. csr-todo's search box
// drives `searchTodos` (a `stream()` RPC, kind: "stream") via an async iterator
// in App.tsx: each yielded Todo is appended to React state and rendered as a
// new <li>. searchTodos is declared `auth: anon, publiclyAccessible: true`
// (ISS-69), so the anonymous browser reaches it through the gateway.
//
// These specs caught ISS-71. Two distinct issues, isolated with raw-fetch probes:
//   (a) a stale local `sdks/rpc/dist` shipped a stream consumer that stalled
//       after the first frame — fixed by rebuilding the SDK (dist is gitignored;
//       the source was already correct, so not a committed bug); and
//   (b) a runtime stream-lifecycle bug (FIXED): the 2nd+ streamed response on an
//       isolate stalled after its first frame because a streamed dispatch didn't
//       wake the pump when it had gone idle after a prior request. A pure-runtime
//       RED test pinned it (crates/runtime/tests/iss71_sequential_streams.rs);
//       fixed by `notify_pump()` on the Stream outcome path. Both specs now pass.
test.describe("RPC streaming (csr-todo searchTodos)", () => {
  test.skip(!appSlug("csr"), "csr-todo not deployed (dist missing)");

  // RELIABLE: the mount default-"build" stream's data frame renders in the
  // browser DOM — proof that anon streaming RPC reaches the browser over the
  // gateway (the transport works end-to-end into client React state).
  test("a streamed match renders in the browser over the gateway", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
    await expect(page.locator("h1")).toHaveText("csr-todo");

    const streamItems = page.locator("section ul li");
    // The default "build" query yields exactly one match; it arrives over the
    // SSE stream and is appended to the rendered list.
    await expect(streamItems).toHaveCount(1);
    await expect(streamItems.nth(0)).toHaveText("Build the CSR demo");
  });

  // Full incremental multi-frame rendering after a re-query (ISS-71b regression).
  // The mount "build" stream is the 1st on the isolate; changing the query starts
  // a 2nd stream — which used to deliver only its first frame and hang, because a
  // streamed dispatch didn't wake the runtime pump if it had gone idle after the
  // prior request (the pump drives the response body's async read loop). Fixed by
  // `notify_pump()` on the Stream outcome path (crates/runtime/src/core/runtime.rs),
  // mirroring the Pending path. Asserts the <li>s climb incrementally to 3 and the
  // stream TERMINATES (status flips to "3 matches").
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
    // (the consumer ended the iterator on completion — the ISS-71 regression).
    await expect(streamSection.getByText(/^3 matches$/)).toBeVisible();

    const intermediates = [...observed].filter((n) => n > 0 && n < 3);
    expect(
      intermediates.length,
      `expected to observe an intermediate streamed frame; saw counts ${[...observed].sort((a, b) => a - b).join(",")}`,
    ).toBeGreaterThan(0);
  });
});
