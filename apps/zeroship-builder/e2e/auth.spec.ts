import { test, expect } from "@playwright/test";

// Auth UI smoke test: public auth pages render and the immersive login
// modal is wired.
//
// IMMERSIVE LOGIN PIVOT. The email/password path is no longer an in-page
// credential form (the old `login-password`/`signup-password` testids are
// gone). It is now the SDK `<AuthModal>`, which hosts a cross-origin,
// same-site iframe embedding `auth.zeroship.ai`'s real `/login` form (the
// Stripe-Elements model). Playwright CANNOT fill the credential inputs on
// the parent page — in prod they live inside the auth-origin frame (SOP);
// the actual credential entry is covered by the live full-stack browser
// e2e (human-run). These specs assert the modal/iframe wiring + the
// federated Google launcher, which is the deterministic offline surface.
//
// No OPENAI_API_KEY required, no control plane required, no sandbox
// controller required. Pure UI smoke tests against a vanilla `npm run dev`.

test.describe("auth UI surfaces", () => {
  test("/login renders the Google launcher, the email trigger, and a link to /signup", async ({
    page,
  }) => {
    await page.goto("/login");

    const root = page.getByTestId("login-page");
    await expect(root).toBeVisible();

    // The federated popup launcher stays a button on the page.
    await expect(page.getByTestId("login-google")).toBeVisible();
    // The email path is now a trigger that opens the immersive modal.
    await expect(page.getByTestId("login-email-trigger")).toBeVisible();

    // Link to signup.
    const signupLink = page.getByTestId("login-link-signup");
    await expect(signupLink).toBeVisible();
    await expect(signupLink).toHaveAttribute("href", "/signup");
  });

  test("/login email trigger opens the immersive AuthModal (dialog + iframe host + close)", async ({
    page,
  }) => {
    await page.goto("/login");

    // The modal is not in the DOM until the trigger fires.
    await expect(page.getByRole("dialog")).toHaveCount(0);

    await page.getByTestId("login-email-trigger").click();

    // The SDK <AuthModal> mounts: role=dialog + aria-modal, an iframe host
    // slot (where the cross-origin auth frame mounts), and an accessible
    // close control. The credential inputs live INSIDE the cross-origin
    // frame and are intentionally NOT fillable from the parent page.
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog).toHaveAttribute("aria-modal", "true");
    await expect(page.getByTestId("auth-iframe-host")).toBeVisible();
    await expect(page.getByTestId("auth-modal-close")).toBeVisible();

    // The old same-origin credential inputs are gone (deleted with the pivot).
    await expect(page.getByTestId("login-password")).toHaveCount(0);
    await expect(page.getByTestId("login-email")).toHaveCount(0);
  });

  test("/login AuthModal close affordance dismisses the dialog", async ({ page }) => {
    await page.goto("/login");
    await page.getByTestId("login-email-trigger").click();
    await expect(page.getByRole("dialog")).toBeVisible();

    await page.getByTestId("auth-modal-close").click();
    await expect(page.getByRole("dialog")).toHaveCount(0);
  });

  test("/signup renders the Google launcher, the email trigger, and a link to /login", async ({
    page,
  }) => {
    await page.goto("/signup");

    const root = page.getByTestId("signup-page");
    await expect(root).toBeVisible();

    await expect(page.getByTestId("signup-google")).toBeVisible();
    await expect(page.getByTestId("signup-email-trigger")).toBeVisible();

    // Link back to /login.
    await expect(page.getByRole("link", { name: /sign in/i })).toBeVisible();
  });

  test("/signup email trigger opens the immersive AuthModal", async ({ page }) => {
    await page.goto("/signup");
    await expect(page.getByRole("dialog")).toHaveCount(0);

    await page.getByTestId("signup-email-trigger").click();

    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog).toHaveAttribute("aria-modal", "true");
    await expect(page.getByTestId("auth-iframe-host")).toBeVisible();
    await expect(page.getByTestId("auth-modal-close")).toBeVisible();

    // The old same-origin credential inputs are gone.
    await expect(page.getByTestId("signup-password")).toHaveCount(0);
    await expect(page.getByTestId("signup-email")).toHaveCount(0);
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
