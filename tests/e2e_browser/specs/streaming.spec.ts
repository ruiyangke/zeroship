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
//   (b) a runtime/isolate stream-lifecycle bug (OPEN): the Nth streamed response
//       on an isolate, and any stream started after a prior stream was aborted
//       mid-flight, stalls after its first frame. A single stream on a fresh
//       connection always works. H1 keep-alive connection reuse is a secondary
//       aggravator, but `force_close()` on streamed responses did NOT fix the
//       browser re-query, so the root is the runtime stream lifecycle, not the
//       connection. See ISS-71b for the full evidence.
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

  // FIXME(ISS-71b): full incremental multi-frame rendering after a re-query.
  // The mount "build" stream is aborted mid-flight when the query changes, then
  // the "the" stream starts — and stalls after its first frame (a runtime/isolate
  // stream-lifecycle bug; a single fresh stream always works). Flip back to
  // `test(` once the runtime cleanly tears down an aborted/completed stream so a
  // subsequent stream on the same isolate streams all its frames.
  test.fixme("typing a query streams matches into the DOM one frame at a time", async ({ page }) => {
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
