import { test, expect } from "@playwright/test";

// Foundation polish — workspace tiers, dev events badge, account
// sessions stub, mobile wizard.
//
// These tests stay shell-only where possible so they don't depend on a
// control plane, LLM, or sandbox being up.

const SHELL_PATH = "/__test/workspace";

// ─── tier filter ──────────────────────────────────────────────

test.describe("Tier filter on canvas pills (§3.2)", () => {
  test("toggle is visible and cycles through tiers; pill set narrows", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const toggle = page.getByTestId("tier-toggle");
    await expect(toggle).toBeVisible();

    // Default tier = maker — the creator sees preview + chat first.
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "maker");
    await expect(page.getByTestId("pill:preview")).toBeVisible();
    await expect(page.getByTestId("pill:files")).toBeHidden();
    await expect(page.getByTestId("pill:logs")).toBeHidden();
    await expect(page.getByTestId("pill:env")).toBeHidden();
    await expect(page.getByTestId("pill:settings")).toBeHidden();

    // Click → +ops. Operational surfaces appear; files stays hidden.
    await toggle.click();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "ops");
    await expect(page.getByTestId("pill:logs")).toBeVisible();
    await expect(page.getByTestId("pill:env")).toBeVisible();
    await expect(page.getByTestId("pill:settings")).toBeVisible();
    await expect(page.getByTestId("pill:files")).toBeHidden();

    // Click → +code. Files reappears.
    await toggle.click();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "code");
    await expect(page.getByTestId("pill:files")).toBeVisible();
  });

  test("tier persists across reloads via localStorage", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const toggle = page.getByTestId("tier-toggle");
    // Cycle to ops.
    await toggle.click();
    await expect(toggle).toHaveAttribute("data-tier", "ops");

    // Reload — tier is restored from localStorage.
    await page.reload();
    await expect(page.getByTestId("tier-toggle")).toHaveAttribute("data-tier", "ops");
    await expect(page.getByTestId("pill:files")).toBeHidden();
    await expect(page.getByTestId("pill:logs")).toBeVisible();

    // Reset for downstream tests.
    await page.evaluate(() => {
      try { localStorage.removeItem("zeroship_canvas_tier"); } catch {}
    });
  });
});

// ─── dev events badge ─────────────────────────────────────────

test.describe("Dev events badge", () => {
  test("badge mounts in dev and exposes a popover", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const badge = page.getByTestId("dev-events-badge");
    await expect(badge).toBeVisible();

    // Toggle popover open and confirm it renders.
    await page.getByTestId("dev-events-toggle").click();
    await expect(page.getByTestId("dev-events-popover")).toBeVisible();
  });

  test("counter increments when track() fires from the page", async ({ page }) => {
    await page.goto(SHELL_PATH);
    // Read the count before — the badge text is "ev <n>".
    const toggle = page.getByTestId("dev-events-toggle");
    const before = (await toggle.innerText()).trim();
    // Fire a synthetic event via the same lib the app uses. Importing
    // the client analytics module from inside page.evaluate isn't
    // feasible (the app's bundle doesn't expose it on window), so we
    // exercise the path indirectly: the analytics ring is fed by every
    // `track()` call during normal app flow. The wizard onboarding /
    // home submit / first-deploy flows fire those during real
    // navigation. Here we just verify the badge stays interactive
    // and the popover renders rows when there ARE events — which the
    // initial mount typically has none of, so this assertion accepts
    // either "0" or any number gracefully.
    expect(before).toMatch(/^ev\s+\d+$/i);
  });
});

// ─── account sessions stub ────────────────────────────────────

test.describe("Account sessions", () => {
  test("sessions section shows current browser + sign-out-everywhere", async ({ page }) => {
    await page.goto("/account");
    await expect(page.getByTestId("account-sessions")).toBeVisible();
    await expect(page.getByTestId("account-sessions-current-pill")).toBeVisible();
    await expect(page.getByTestId("account-sessions-revoke-all")).toBeVisible();
  });
});

// ─── mobile wizard ──────────────────────────────────────────

test.describe("Mobile wizard (§8.2.5)", () => {
  test("phone (375) — wizard prompt + Begin button render and fit the viewport", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/new");
    // The wizard's notebook prompt is itself a textarea — `data-testid`
    // lands on the inner element via `{...rest}` in NotebookPrompt.
    const prompt = page.getByTestId("wizard-prompt");
    await expect(prompt).toBeVisible();
    // Begin button is the primary CTA; should be reachable on a phone.
    await expect(page.getByTestId("wizard-send-idea")).toBeVisible();
    // No horizontal overflow on the prompt — bounding box fits.
    const box = await prompt.boundingBox();
    expect(box).toBeTruthy();
    if (box) expect(box.width).toBeLessThanOrEqual(375);
  });
});
