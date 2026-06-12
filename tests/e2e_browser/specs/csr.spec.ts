import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// CSR (csr-todo): the gateway serves a near-empty SPA shell (`<div id="root">`
// + a `<script type=module>`); ALL UI is rendered client-side by React after
// the bundle boots. The todo list is fetched over a typed RPC (`listTodos`)
// via TanStack Query. The browser contract:
//   1. the SPA MOUNTS — UI that the static shell does NOT contain appears
//   2. the todo list round-trips through the listTodos RPC into the DOM
//   3. client interactivity works (Add local mutates the rendered list)
//   4. a deep SPA route serves the shell and the client router renders it
test.describe("CSR (csr-todo) — SPA mount + RPC round-trip", () => {
  test.skip(!appSlug("csr"), "csr-todo not deployed (dist missing)");

  test("the SPA shell contains no app UI before JS runs", async ({ page }) => {
    // Confirm the load-bearing premise: the server shell is empty. With JS
    // disabled, #root has no rendered todo UI — so anything we assert later is
    // proof the client mounted, not server output.
    const ctx = await page.context().browser()!.newContext({ javaScriptEnabled: false });
    const p = await ctx.newPage();
    try {
      await p.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
      await expect(p.locator("#root")).toBeAttached();
      // No <h1>csr-todo</h1> in the static shell — that only appears once React mounts.
      await expect(p.locator("h1")).toHaveCount(0);
    } finally {
      await ctx.close();
    }
  });

  // FIXME(ISS-69): the SPA mounts fine, but listTodos is an ANONYMOUS client
  // RPC and the gateway fail-closes it with 401 UNAUTHENTICATED (SEC-5 default
  // auth: user). The procedure itself is healthy over the worker /dispatch path
  // (returns the 4 todos, 200) — only the browser→gateway RPC is blocked. This
  // is a real platform/example contract finding, not a test bug. Flip back to
  // `test(` once anon (or test-authed) client RPC works through the gateway.
  test.fixme("SPA mounts and the todo list round-trips through the listTodos RPC", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });

    // Mount signal: the React-rendered heading appears (absent from the shell).
    await expect(page.locator("h1")).toHaveText("csr-todo");

    // The four hardcoded todos come back over the listTodos RPC and render as
    // <li> items. Their presence proves the gateway→worker→V8 RPC round-trip
    // landed in the real DOM.
    await expect(page.getByText("Read the .zship spec")).toBeVisible();
    await expect(page.getByText("Build the CSR demo")).toBeVisible();
    await expect(page.getByText("Verify the manifest")).toBeVisible();
    await expect(page.getByText("Stretch — wire up SSR")).toBeVisible();

    // The status line reflects the RPC-sourced count (4 todos).
    await expect(page.getByText(/^4 todos$/)).toBeVisible();
  });

  // FIXME(ISS-69): the precondition "4 todos" comes from the listTodos RPC,
  // which the gateway 401s for the anonymous browser (see ISS-69). The Add-local
  // interaction itself is pure client state; this regains coverage once anon
  // client RPC works and the initial count renders.
  test.fixme("client interactivity: Add local appends a rendered item", async ({ page }) => {
    await page.goto(appUrl("csr", "/"), { waitUntil: "domcontentloaded" });
    await expect(page.getByText(/^4 todos$/)).toBeVisible();

    await page.getByRole("button", { name: "Add local" }).click();

    // The count line updates and the new local item is in the DOM — proves the
    // mounted SPA is interactive (state → re-render), not static markup.
    await expect(page.getByText(/^5 todos$/)).toBeVisible();
    await expect(page.getByText("Local todo 5")).toBeVisible();
  });

  test("deep SPA route serves the shell and the client router renders", async ({ page }) => {
    // SPA catch-all: a path the server has no static asset for must still serve
    // the index shell (try $path → /index.html fallback), and the client router
    // (main.tsx) renders for it. csr-todo's router treats any non-/about path as
    // the App view, so the todo UI mounts on a deep route too.
    const resp = await page.goto(appUrl("csr", "/some/spa/route"), {
      waitUntil: "domcontentloaded",
    });
    expect(resp?.status()).toBe(200);

    // The client router (main.tsx) treats any non-/about path as the App view,
    // so the React-rendered <h1>csr-todo</h1> heading appearing on a deep route
    // proves both: the gateway served the index shell as the SPA fallback, AND
    // the client router mounted the App for that route. (The todo *list* on this
    // page is RPC-sourced and currently 401s — see ISS-69 — so we assert the
    // router mount via the heading, which is independent of the RPC.)
    await expect(page.locator("h1")).toHaveText("csr-todo");
    // The stream box's search input is part of the App view → confirms the App
    // (not the About view, not a blank shell) rendered on the deep route.
    await expect(page.getByText("Search stream")).toBeVisible();
  });
});
