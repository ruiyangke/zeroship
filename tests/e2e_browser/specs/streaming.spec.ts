import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// RPC streaming, consumed CLIENT-SIDE in the browser. csr-todo's search box
// drives `searchTodos` (a `stream()` RPC, kind: "stream") via an async iterator
// in App.tsx: each yielded Todo is appended to React state and rendered as a
// new <li>. searchTodos is declared `auth: anon, publiclyAccessible: true`
// (ISS-69), so the anonymous browser reaches it through the gateway.
//
// What works (passing test below): a stream's data frame reaches the browser
// DOM over the real gateway→worker→V8 SSE path.
//
// What's BROKEN — ISS-71 (fixme test below): the @zeroship/rpc browser stream
// consumer never terminates on body-close and surfaces only the FIRST frame.
// Evidence: curl through the gateway delivers every data frame (+ close) for a
// 3-match query, but in the browser only frame 1 renders and the status stays
// "streaming…" forever (even a 1-match stream never flips to "1 matches"). So
// full incremental multi-frame rendering + done-status can't be asserted in a
// browser until ISS-71 lands. The HTTP-layer incremental proof (3 frames ~30ms
// apart) lives in the curl harness (tests/e2e_app_primitives_render.sh).
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

  // FIXME(ISS-71): full incremental multi-frame rendering. The server streams
  // 3 data frames for "the" ~30ms apart (curl-proven through the gateway), but
  // the browser client surfaces only the first and never terminates the stream
  // (status stuck "streaming…"). Flip back to `test(` once the @zeroship/rpc
  // stream consumer yields every frame and ends on body-close.
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

    const intermediates = [...observed].filter((n) => n > 0 && n < 3);
    expect(
      intermediates.length,
      `expected to observe an intermediate streamed frame; saw counts ${[...observed].sort((a, b) => a - b).join(",")}`,
    ).toBeGreaterThan(0);
  });
});
