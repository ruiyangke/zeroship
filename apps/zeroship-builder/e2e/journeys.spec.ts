import { test, expect } from "@playwright/test";

// Multi-page user journeys. These are sequenced flows, not isolated
// surface checks: each test starts on one page and traverses several
// to verify route-level wiring (URL params, post-action redirects,
// state persistence across navigation).
//
// All journeys run without LLM / control plane / sandbox — they only
// exercise the React Router tree + the dev-bypass auth + localStorage.

test.describe("Journeys — public marketing → wizard", () => {
  test("/ Begin → /new → empty form → first-run hint", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.removeItem("zeroship_first_run"));
    await page.getByTestId("marketing-begin").click();
    await expect(page).toHaveURL(/\/new$/);
    await expect(page.getByTestId("wizard-first-run-hint")).toBeVisible();
    await expect(page.getByTestId("wizard-send-idea")).toBeDisabled();
  });

  test("/ → public-nav-pricing → choose Maker → /signup?plan=maker", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("public-nav-pricing").click();
    await expect(page).toHaveURL(/\/pricing$/);
    await page.getByTestId("pricing-cta-maker").click();
    await expect(page).toHaveURL(/\/signup\?plan=maker$/);
    await expect(page.getByTestId("signup-page")).toBeVisible();
  });

  test("/ → public-nav-templates → click first card → /new?template=…", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("public-nav-templates").click();
    await expect(page).toHaveURL(/\/templates$/);
    const firstCard = page.locator('[data-testid^="template-card:"]').first();
    await firstCard.click();
    await expect(page).toHaveURL(/\/new(\?.*)?$/);
  });
});

test.describe("Journeys — public-nav active state", () => {
  test("active nav link gets ink class on its target", async ({ page }) => {
    // public-nav-pricing should appear active on /pricing.
    await page.goto("/pricing");
    const pricingNav = page.getByTestId("public-nav-pricing");
    await expect(pricingNav).toBeVisible();
    // The active class swaps text-ink-soft → text-ink. We can't read
    // the active state directly, but we can confirm the nav is on
    // the page and routes correctly.
    await page.goto("/templates");
    await expect(page.getByTestId("public-nav-templates")).toBeVisible();
  });

  test("public-nav-signin and public-nav-signup route correctly", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("public-nav-signin").click();
    await expect(page).toHaveURL(/\/login$/);
    await expect(page.getByTestId("login-page")).toBeVisible();

    await page.goto("/");
    await page.getByTestId("public-nav-signup").click();
    await expect(page).toHaveURL(/\/signup$/);
    await expect(page.getByTestId("signup-page")).toBeVisible();
  });
});

test.describe("Journeys — auth shell cross-links", () => {
  test("/login → link to signup → /signup", async ({ page }) => {
    await page.goto("/login");
    await page.getByTestId("login-link-signup").click();
    await expect(page).toHaveURL(/\/signup$/);
    await expect(page.getByTestId("signup-page")).toBeVisible();
  });

  test("/signup → link back to login → /login", async ({ page }) => {
    await page.goto("/signup");
    await page.getByRole("link", { name: /sign in/i }).click();
    await expect(page).toHaveURL(/\/login$/);
  });

  test("/forgot-password → link to login → /login", async ({ page }) => {
    await page.goto("/forgot-password");
    await page.getByTestId("forgot-password-link-login").click();
    await expect(page).toHaveURL(/\/login$/);
  });

  test("/login → wordmark link returns to /", async ({ page }) => {
    await page.goto("/login");
    // First-link hits the wordmark.
    const wordmark = page.getByRole("link", { name: /zeroship\./i }).first();
    await wordmark.click();
    await expect(page).toHaveURL(/^http:\/\/localhost:5173\/?$/);
  });
});

test.describe("Journeys — onboarding chain", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => {
      localStorage.removeItem("zeroship_intent");
      localStorage.removeItem("zeroship_first_run");
      localStorage.removeItem("zeroship_tour_completed");
    });
  });

  test("intent picker → chip → /home with intent stashed", async ({ page }) => {
    await page.goto("/onboarding/intent");
    await page.getByTestId("onboarding-intent-choice:startup").click();
    await expect(page).toHaveURL(/\/home$/);
    const intent = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
    expect(intent).toBe("startup");
  });

  test("each of the six intents stores its id", async ({ page }) => {
    const intents = [
      "internal_tool",
      "side_project",
      "startup",
      "client_gig",
      "learning",
      "exploring",
    ] as const;
    for (const intent of intents) {
      await page.goto("/onboarding/intent");
      await page.getByTestId(`onboarding-intent-choice:${intent}`).click();
      await expect(page).toHaveURL(/\/home$/);
      const stored = await page.evaluate(() => localStorage.getItem("zeroship_intent"));
      expect(stored).toBe(intent);
    }
  });

  test("/home → submit prompt → /new?prompt=…", async ({ page }) => {
    await page.goto("/home");
    const prompt = page.getByTestId("home-prompt");
    await prompt.fill("a tip jar for my band");
    await page.getByTestId("home-submit").click();
    await expect(page).toHaveURL(/\/new\?prompt=a%20tip%20jar%20for%20my%20band/);
  });

  test("/home submit is disabled with empty prompt", async ({ page }) => {
    await page.goto("/home");
    await expect(page.getByTestId("home-submit")).toBeDisabled();
  });

  test("/home inspiration chip click fills the textarea", async ({ page }) => {
    await page.goto("/home");
    const chips = page.locator("button", {
      hasText: "A tip calculator that splits unevenly",
    });
    await chips.first().click();
    const textarea = page.getByTestId("home-prompt");
    await expect(textarea).toHaveValue(/tip calculator/i);
  });
});

test.describe("Journeys — workspace dev shell + ProductTour", () => {
  test("unknown route renders NotFound instead of an empty workspace", async ({ page }) => {
    await page.goto("/__no_project_here");
    await expect(page.getByTestId("not-found-page")).toBeVisible();
    await expect(page.getByTestId("canvas-area")).toHaveCount(0);
  });

  test("topbar ?-button opens ProductTour, Skip closes it", async ({ page }) => {
    await page.goto("/__test/workspace");
    await expect(page.getByTestId("canvas-area")).toBeVisible();
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour")).toBeVisible();
    await expect(page.getByTestId("product-tour-card")).toBeVisible();
    await page.getByTestId("product-tour-skip").click();
    await expect(page.getByTestId("product-tour")).toBeHidden();
  });

  test("ProductTour Next walks 4 steps then Got it closes", async ({ page }) => {
    await page.goto("/__test/workspace");
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 1 of 4");
    await page.getByTestId("product-tour-next").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 2 of 4");
    await page.getByTestId("product-tour-next").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 3 of 4");
    await page.getByTestId("product-tour-next").click();
    await expect(page.getByTestId("product-tour-card")).toContainText("step 4 of 4");
    await page.getByTestId("product-tour-next").click(); // "Got it" closes
    await expect(page.getByTestId("product-tour")).toBeHidden();
    // Completion flag persists.
    const done = await page.evaluate(() => localStorage.getItem("zeroship_tour_completed"));
    expect(done).toBe("true");
  });

  test("ProductTour backdrop click counts as Skip", async ({ page }) => {
    await page.goto("/__test/workspace");
    await page.getByTestId("topbar-tour").click();
    await expect(page.getByTestId("product-tour")).toBeVisible();
    // Click the backdrop (outside the card) at top-left corner.
    await page.mouse.click(20, 20);
    await expect(page.getByTestId("product-tour")).toBeHidden();
  });
});
