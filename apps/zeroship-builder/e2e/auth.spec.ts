import { test, expect } from "@playwright/test";

// Auth UI smoke test: public auth pages render and protected routes
// is wired. These tests do NOT exercise the control plane: they only
// assert that the public auth pages render correctly and that the
// dev-bypass branch of AuthGuard lets a developer hit /account.
//
// No OPENAI_API_KEY required, no control plane required, no sandbox
// controller required. These are pure UI smoke tests that should run
// against a vanilla `npm run dev` worktree.

test.describe("auth UI surfaces", () => {
  test("/login renders the form, Google button, and link to /signup", async ({ page }) => {
    await page.goto("/login");

    const root = page.getByTestId("login-page");
    await expect(root).toBeVisible();

    await expect(page.getByTestId("login-email")).toBeVisible();
    await expect(page.getByTestId("login-password")).toBeVisible();
    await expect(page.getByTestId("login-submit")).toBeVisible();
    await expect(page.getByTestId("login-google")).toBeVisible();

    // Google CTA points at /auth/google/start (sync URL builder).
    const googleHref = await page.getByTestId("login-google").getAttribute("href");
    expect(googleHref).toMatch(/^\/auth\/google\/start\?/);

    // Link to signup.
    const signupLink = page.getByTestId("login-link-signup");
    await expect(signupLink).toBeVisible();
    await expect(signupLink).toHaveAttribute("href", "/signup");
  });

  test("/signup renders name + email + password + Google + link to /login", async ({ page }) => {
    await page.goto("/signup");

    const root = page.getByTestId("signup-page");
    await expect(root).toBeVisible();

    await expect(page.getByTestId("signup-email")).toBeVisible();
    await expect(page.getByTestId("signup-password")).toBeVisible();
    await expect(page.getByTestId("signup-name")).toBeVisible();
    await expect(page.getByTestId("signup-submit")).toBeVisible();
    await expect(page.getByTestId("signup-google")).toBeVisible();

    // Link back to /login.
    await expect(page.getByRole("link", { name: /sign in/i })).toBeVisible();
  });

  test("/forgot-password renders email field + submit", async ({ page }) => {
    await page.goto("/forgot-password");

    const root = page.getByTestId("forgot-password-page");
    await expect(root).toBeVisible();

    const email = page.getByTestId("forgot-password-email");
    const submit = page.getByTestId("forgot-password-submit");
    await expect(email).toBeVisible();
    await expect(submit).toBeVisible();

    // Submit enables once an email is filled in, then the page swaps
    // to the no-enumeration confirmation reply.
    await email.fill("test@example.com");
    await submit.click();
    await expect(page.getByTestId("forgot-password-confirmation")).toBeVisible();
  });

  test("/account loads in dev mode (devBypass) without a real login", async ({ page }) => {
    await page.goto("/account");

    // AuthGuard's devBypass branch lets us through; the Account page
    // renders the full layout with the dev-user identity baked in by
    // AuthProvider's DEV_USER constant.
    await expect(page.getByTestId("account-page")).toBeVisible();
    await expect(page.getByTestId("account-logout")).toBeVisible();

    // Identity card pulls the synthetic dev user.
    await expect(page.getByTestId("account-email")).toHaveValue("dev@localhost");

    // Deferred sections show their "Coming soon" stubs.
    await expect(page.getByTestId("account-sessions")).toBeVisible();
    await expect(page.getByTestId("account-2fa")).toBeVisible();
    await expect(page.getByTestId("account-delete")).toBeVisible();
  });
});
