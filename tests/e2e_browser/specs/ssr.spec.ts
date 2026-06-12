import { test, expect } from "@playwright/test";
import { appSlug, appUrl } from "../helpers";

// SSR (ssr-blog): each GET is rendered per-request in V8 (react-dom/server
// renderToString), shipped as HTML with a `window.__SSR_PROPS__` island plus a
// `<script type=module>` client entry that calls hydrateRoot(). The browser
// contract:
//   1. server-rendered post content is present on first paint (before any
//      client JS could have run)
//   2. hydration ACTUALLY BOOTS — the client bundle executes and makes the
//      page interactive (an onClick that only exists post-hydration fires).
test.describe("SSR (ssr-blog) — per-request HTML + client hydration", () => {
  test.skip(!appSlug("ssr"), "ssr-blog not deployed (dist missing)");

  test("GET / shows server-rendered post list immediately", async ({ page }) => {
    // Inspect the raw response too: the rendered post title must be in the HTML
    // the server sent, not injected later by client JS.
    const resp = await page.goto(appUrl("ssr", "/"), { waitUntil: "domcontentloaded" });
    expect(resp?.status()).toBe(200);
    const rawHtml = await resp!.text();
    expect(rawHtml).toContain("Why SSR is back in fashion");
    expect(rawHtml).toContain("__SSR_PROPS__");
    expect(rawHtml).toMatch(/<script[^>]*type="module"/);

    await expect(page.locator("h1")).toHaveText("ssr-blog");
    await expect(
      page.getByRole("link", { name: "Why SSR is back in fashion" }),
    ).toBeVisible();
  });

  test("hydration boots: a post page becomes interactive", async ({ page }) => {
    // The single-post page (<Post>) renders prev/next buttons whose onClick
    // handlers are wired ONLY after hydrateRoot() runs. We pick `/post/ssg-vs-ssr`
    // (the middle post) so "← Prev" is enabled and navigates to the first post.
    const resp = await page.goto(appUrl("ssr", "/post/ssg-vs-ssr"), {
      waitUntil: "domcontentloaded",
    });
    expect(resp?.status()).toBe(200);

    // Server-rendered single-post content is present on first paint.
    await expect(page.locator("h1")).toHaveText("SSG vs SSR — pick on traffic shape");

    // Prove the client bundle executed at all: __SSR_PROPS__ is set by the
    // server, and after hydration React has taken over #root. Wait for the
    // hydration signal — the client entry only runs when props exist and the
    // module loads, so we assert the prev button responds to a real click.
    const prev = page.getByRole("button", { name: "← Prev" });
    await expect(prev).toBeEnabled();

    // Clicking "← Prev" calls window.location.assign(`/post/first`) — an onClick
    // that exists ONLY after hydration. A static (un-hydrated) button does
    // nothing. A successful navigation to /post/first proves hydration booted.
    await Promise.all([
      page.waitForURL(/\/post\/first$/, { timeout: 20_000 }),
      prev.click(),
    ]);
    await expect(page.locator("h1")).toHaveText("Why SSR is back in fashion");
  });
});
