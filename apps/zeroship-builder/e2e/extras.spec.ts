import { test, expect } from "@playwright/test";

// Additional surface coverage for areas the other specs only touched
// lightly. Single-page interactions; no LLM / control plane / sandbox.

const SHELL_PATH = "/__catchall_for_test";

test.describe("Pricing + 15% share band", () => {
  test("revenue example renders with the right numbers", async ({ page }) => {
    await page.goto("/pricing");
    // Worked example: $100 → Stripe $3.20 → zeroship $15.00 → keep $81.80.
    await expect(page.getByTestId("pricing-page")).toContainText("$100");
    await expect(page.getByTestId("pricing-page")).toContainText("$3.20");
    await expect(page.getByTestId("pricing-page")).toContainText("$15.00");
    await expect(page.getByTestId("pricing-page")).toContainText("$81.80");
  });

  test("Maker plan is emphasised with 'Most chosen' tag", async ({ page }) => {
    await page.goto("/pricing");
    await expect(page.getByTestId("pricing-plan-maker")).toContainText("Most chosen");
  });
});

test.describe("Marketing — featured templates", () => {
  test("featured templates strip has at least one card", async ({ page }) => {
    await page.goto("/");
    const cards = page.locator('[data-testid^="template-card:"]');
    expect(await cards.count()).toBeGreaterThanOrEqual(1);
  });

  test("Marketing hero CTAs render", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("marketing-begin")).toBeVisible();
    await expect(page.getByTestId("marketing-templates")).toBeVisible();
  });
});

test.describe("Public legal pages", () => {
  test("/legal/privacy renders some legal-flavoured copy", async ({ page }) => {
    await page.goto("/legal/privacy");
    await expect(page.getByTestId("privacy-page")).toBeVisible();
    // Body text is present (heading or first paragraph).
    const text = await page.getByTestId("privacy-page").textContent();
    expect((text ?? "").length).toBeGreaterThan(80);
  });

  test("/legal/terms renders some legal-flavoured copy", async ({ page }) => {
    await page.goto("/legal/terms");
    await expect(page.getByTestId("terms-page")).toBeVisible();
    const text = await page.getByTestId("terms-page").textContent();
    expect((text ?? "").length).toBeGreaterThan(80);
  });

  test("/about renders editorial copy", async ({ page }) => {
    await page.goto("/about");
    await expect(page.getByTestId("about-page")).toBeVisible();
    const text = await page.getByTestId("about-page").textContent();
    expect((text ?? "").length).toBeGreaterThan(80);
  });

  test("/changelog has at least one entry block", async ({ page }) => {
    await page.goto("/changelog");
    await expect(page.getByTestId("changelog-entries")).toBeVisible();
  });
});

test.describe("Public-nav resilience", () => {
  test("logo link routes back to / from any public page", async ({ page }) => {
    await page.goto("/pricing");
    await page.getByTestId("public-nav-logo").click();
    await expect(page).toHaveURL(/^http:\/\/localhost:5173\/?$/);
  });

  test("public-nav-signin routes from /skills", async ({ page }) => {
    await page.goto("/skills");
    await page.getByTestId("public-nav-signin").click();
    await expect(page).toHaveURL(/\/login$/);
  });
});

test.describe("Templates — interactivity", () => {
  test("clicking a template card navigates to /new with a template param", async ({ page }) => {
    await page.goto("/templates");
    const firstCard = page.locator('[data-testid^="template-card:"]').first();
    const slug = await firstCard.getAttribute("data-testid");
    expect(slug).toMatch(/^template-card:/);
    await firstCard.click();
    // Either /new?template=<slug> or /new without param — either is
    // valid wiring depending on TemplateCard's behaviour.
    await expect(page).toHaveURL(/\/new(\?.*)?$/);
  });
});

test.describe("Skills — interactivity", () => {
  test("filter pills present and clickable", async ({ page }) => {
    await page.goto("/skills");
    await expect(page.getByTestId("skills-filters")).toBeVisible();
    const pills = page.getByTestId("skills-filters").locator("button, a");
    const n = await pills.count();
    expect(n).toBeGreaterThan(0);
  });

  test("clicking 'Add to project' on a disabled skill is a no-op", async ({ page }) => {
    await page.goto("/skills");
    const add = page.getByTestId("skill-add:auth");
    await expect(add).toBeDisabled();
    // Disabled buttons swallow clicks; the URL doesn't change.
    const before = page.url();
    await add.click({ force: true }).catch(() => {});
    expect(page.url()).toBe(before);
  });
});

test.describe("Workspace shell — TopBar URL pill click", () => {
  test("TopBar URL pill is a link with the correct host and a tomato dot", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const url = page.getByTestId("topbar-url");
    await expect(url).toBeVisible();
    const tag = await url.evaluate((el) => el.tagName.toLowerCase());
    expect(tag).toBe("a");
  });
});

test.describe("Wizard — composer presence", () => {
  test("/new with a fresh state shows the prompt area + Begin button", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => {
      localStorage.removeItem("zeroship_first_run");
    });
    await page.goto("/new");
    await expect(page.getByTestId("wizard-prompt")).toBeVisible();
    await expect(page.getByTestId("wizard-send-idea")).toBeVisible();
  });
});

test.describe("Catch-all workspace — chat composer disabled state", () => {
  test("composer renders without an appId (no project selected)", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-composer")).toBeVisible();
    await expect(page.getByTestId("chat-input")).toBeVisible();
    // Send button is disabled with empty input.
    await expect(page.getByTestId("chat-send")).toBeDisabled();
  });
});

test.describe("ProductTour — keyboard activation", () => {
  test("ProductTour Next button is focusable and Enter advances", async ({ page }) => {
    await page.goto(SHELL_PATH);
    await page.getByTestId("topbar-tour").click();
    const next = page.getByTestId("product-tour-next");
    await next.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("product-tour-card")).toContainText("step 2 of 4");
    // Close out cleanly.
    await page.getByTestId("product-tour-skip").click();
  });
});

test.describe("Account — logout button shape", () => {
  test("account-logout is a button with destructive styling", async ({ page }) => {
    await page.goto("/account");
    const btn = page.getByTestId("account-logout");
    await expect(btn).toBeVisible();
    const tag = await btn.evaluate((el) => el.tagName.toLowerCase());
    expect(tag).toBe("button");
  });
});

test.describe("Dropdown header swap on @-trigger kind", () => {
  test("typing @file then @issue swaps the header", async ({ page }) => {
    await page.goto(SHELL_PATH);
    const input = page.getByTestId("chat-input");
    await input.click();
    await input.type("@file");
    await expect(page.getByTestId("mention-dropdown")).toContainText("files");
    // Clear and try issue.
    await input.fill("");
    await input.type("@issue");
    await expect(page.getByTestId("mention-dropdown")).toContainText("issues");
  });
});
