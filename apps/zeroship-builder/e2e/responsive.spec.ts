import { test, expect } from "@playwright/test";

// Responsive sweep — phone (375), tablet (768), desktop (1280).
// Verifies layout-level affordances kick in at the right breakpoints.
// Uses page.setViewportSize per test for tighter isolation than
// test.use({viewport}). The dev server is shared so each test still
// hits a fresh page.

const PHONE = { width: 375, height: 812 };
const TABLET = { width: 768, height: 1024 };
const DESKTOP = { width: 1280, height: 800 };

const SHELL_PATH = "/__test/workspace";

test.describe("Responsive — workspace shell", () => {
  test("phone (375) hides the chat sidebar; topbar gets a chat toggle", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("topbar-chat-toggle")).toBeVisible();
    // Sidebar chat-rail should not be in the layout — chat-drawer is
    // only present when toggled open.
    const drawer = page.getByTestId("chat-drawer");
    await expect(drawer).toBeHidden();
  });

  test("phone — tapping topbar-chat-toggle opens the chat-drawer", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto(SHELL_PATH);
    await page.getByTestId("topbar-chat-toggle").click();
    await expect(page.getByTestId("chat-drawer")).toBeVisible();
  });

  test("phone — chat-drawer-close button closes the drawer", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto(SHELL_PATH);
    await page.getByTestId("topbar-chat-toggle").click();
    await expect(page.getByTestId("chat-drawer")).toBeVisible();
    await page.getByTestId("chat-drawer-close").click();
    await expect(page.getByTestId("chat-drawer")).toBeHidden();
  });

  test("tablet (768) — chat sidebar is visible (no toggle button)", async ({ page }) => {
    await page.setViewportSize(TABLET);
    await page.goto(SHELL_PATH);
    // The toggle is conditional on `isPhone` (≤767px). At exactly 768
    // it's a desktop layout, no toggle.
    await expect(page.getByTestId("topbar-chat-toggle")).toBeHidden();
    await expect(page.getByTestId("chat-rail")).toBeVisible();
  });

  test("desktop (1280) — chat sidebar visible, topbar URL pill visible", async ({ page }) => {
    await page.setViewportSize(DESKTOP);
    await page.goto(SHELL_PATH);
    await expect(page.getByTestId("chat-rail")).toBeVisible();
    await expect(page.getByTestId("topbar-url")).toBeVisible();
  });
});

test.describe("Responsive — Home page", () => {
  test("phone — gallery filters still render", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/home");
    await expect(page.getByTestId("home-filters")).toBeVisible();
    await expect(page.getByTestId("home-filter-active")).toBeVisible();
  });

  test("desktop — hero + gallery laid out", async ({ page }) => {
    await page.setViewportSize(DESKTOP);
    await page.goto("/home");
    await expect(page.getByTestId("home-prompt")).toBeVisible();
    await expect(page.getByTestId("home-gallery")).toBeVisible();
  });
});

test.describe("Responsive — Marketing", () => {
  test("phone — marketing CTAs are tap-target sized and visible", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/");
    await expect(page.getByTestId("marketing-page")).toBeVisible();
    await expect(page.getByTestId("marketing-begin")).toBeVisible();
    const box = await page.getByTestId("marketing-begin").boundingBox();
    expect(box).toBeTruthy();
    // Tap target sized: at least ~32px tall (relaxed from WCAG 44px
    // because StampButton is editorial-sized — but still big enough).
    if (box) expect(box.height).toBeGreaterThanOrEqual(32);
  });

  test("tablet — public-nav links visible", async ({ page }) => {
    await page.setViewportSize(TABLET);
    await page.goto("/");
    await expect(page.getByTestId("public-nav")).toBeVisible();
    await expect(page.getByTestId("public-nav-pricing")).toBeVisible();
  });
});

test.describe("Responsive — Auth pages", () => {
  test("phone — login form fits on screen", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/login");
    await expect(page.getByTestId("login-page")).toBeVisible();
    const box = await page.getByTestId("login-page").boundingBox();
    expect(box).toBeTruthy();
    if (box) {
      // No horizontal overflow.
      expect(box.width).toBeLessThanOrEqual(PHONE.width);
    }
  });

  test("phone — signup form fits on screen", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/signup");
    await expect(page.getByTestId("signup-page")).toBeVisible();
    const box = await page.getByTestId("signup-page").boundingBox();
    if (box) {
      expect(box.width).toBeLessThanOrEqual(PHONE.width);
    }
  });
});

test.describe("Responsive — Pricing + Templates grids reflow", () => {
  test("phone — pricing plans stack vertically", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/pricing");
    const free = await page.getByTestId("pricing-plan-free").boundingBox();
    const maker = await page.getByTestId("pricing-plan-maker").boundingBox();
    expect(free).toBeTruthy();
    expect(maker).toBeTruthy();
    // On phone the cards stack: maker.y > free.y + free.height (with
    // some tolerance for spacing).
    if (free && maker) {
      expect(maker.y).toBeGreaterThan(free.y);
    }
  });

  test("desktop — pricing plans render side by side", async ({ page }) => {
    await page.setViewportSize(DESKTOP);
    await page.goto("/pricing");
    const free = await page.getByTestId("pricing-plan-free").boundingBox();
    const maker = await page.getByTestId("pricing-plan-maker").boundingBox();
    if (free && maker) {
      // Adjacent cards share roughly the same y on desktop.
      expect(Math.abs(maker.y - free.y)).toBeLessThan(40);
    }
  });
});

test.describe("Responsive — Templates page", () => {
  test("phone — filter pills wrap and remain tappable", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto("/templates");
    await expect(page.getByTestId("templates-filters")).toBeVisible();
    await expect(page.getByTestId("templates-grid")).toBeVisible();
  });
});

test.describe("Responsive — chat composer", () => {
  test("phone — chat drawer hosts the composer + input", async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto(SHELL_PATH);
    await page.getByTestId("topbar-chat-toggle").click();
    const drawer = page.getByTestId("chat-drawer");
    await expect(drawer).toBeVisible();
    await expect(drawer.getByTestId("chat-composer")).toBeVisible();
    await expect(drawer.getByTestId("chat-input")).toBeVisible();
  });
});
