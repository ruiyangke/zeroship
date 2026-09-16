import { Buffer } from "node:buffer";
import { createHash, createHmac, randomBytes, randomUUID } from "node:crypto";

import {
  expect,
  test,
  type Page,
  type Response,
} from "@playwright/test";
import { authUrl, waitForVerificationUrl } from "../helpers";

const TOTP_TITLE = "TOTP challenge conditionally exposes errors and completes login";
const CONSENT_TITLE =
  "OIDC consent renders scope details, distinguishes actions, and denies access";
const PASSWORD = "correct browser flow password 2026!";

function waitForHttpResponse(page: Page, method: string, path: string): Promise<Response> {
  const expectedOrigin = new URL(authUrl("/")).origin;
  return page.waitForResponse((response) => {
    const url = new URL(response.url());
    return (
      response.request().method() === method &&
      url.origin === expectedOrigin &&
      url.pathname === path
    );
  });
}

async function redeemVerificationLink(page: Page, verificationUrl: string): Promise<Response> {
  const responsePromise = waitForHttpResponse(page, "POST", "/verify/redeem");
  await page.goto(verificationUrl, { waitUntil: "commit" });
  const response = await responsePromise;
  await page.waitForLoadState("domcontentloaded");
  return response;
}

async function createVerifiedSession(page: Page, flowName: string): Promise<{ email: string }> {
  const id = randomUUID();
  const email = `authui-${flowName.toLowerCase()}-${id}@example.test`;
  const signup = await page.goto(authUrl("/signup"));
  expect(signup?.status(), `${flowName} signup GET status`).toBe(200);

  const signupForm = page.locator('form[action="/signup"]');
  await signupForm.locator('input[name="name"]').fill(`${flowName} ${id.slice(0, 8)}`);
  await signupForm.locator('input[name="email"]').fill(email);
  await signupForm.locator('input[name="password"]').fill(PASSWORD);
  const signupResponsePromise = waitForHttpResponse(page, "POST", "/signup");
  await signupForm.getByRole("button", { name: "Sign up" }).click();
  const signupResponse = await signupResponsePromise;
  expect(signupResponse.status(), `${flowName} signup POST status`).toBe(302);

  const verificationUrl = await waitForVerificationUrl(email);
  const verificationResponse = await redeemVerificationLink(page, verificationUrl);
  expect(verificationResponse.status(), `${flowName} verification status`).toBe(200);
  await expect(page.getByRole("heading", { name: "Email verified", exact: true })).toBeVisible();

  const login = await page.goto(authUrl("/login"));
  expect(login?.status(), `${flowName} login GET status`).toBe(200);
  const loginForm = page.locator('form[action="/login"]');
  await loginForm.locator('input[name="email"]').fill(email);
  await loginForm.locator('input[name="password"]').fill(PASSWORD);
  const loginResponsePromise = waitForHttpResponse(page, "POST", "/login");
  await loginForm.getByRole("button", { name: "Sign in" }).click();
  const loginResponse = await loginResponsePromise;
  expect(loginResponse.status(), `${flowName} login POST status`).toBe(303);
  await expect(page, `${flowName} login destination`).toHaveURL(
    (url) => url.pathname === "/me",
  );

  return { email };
}

function decodeBase32(value: string): Buffer {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const normalized = value.trim().replace(/=+$/, "").toUpperCase();
  let bits = "";
  for (const character of normalized) {
    const digit = alphabet.indexOf(character);
    if (digit === -1) {
      throw new Error(`invalid base32 TOTP secret character ${JSON.stringify(character)}`);
    }
    bits += digit.toString(2).padStart(5, "0");
  }

  const bytes: number[] = [];
  for (let offset = 0; offset + 8 <= bits.length; offset += 8) {
    bytes.push(Number.parseInt(bits.slice(offset, offset + 8), 2));
  }
  return Buffer.from(bytes);
}

function totpCode(secretBase32: string, unixSeconds = Math.floor(Date.now() / 1000)): string {
  let counter = BigInt(Math.floor(unixSeconds / 30));
  const message = Buffer.alloc(8);
  for (let index = message.length - 1; index >= 0; index -= 1) {
    message[index] = Number(counter & 0xffn);
    counter >>= 8n;
  }

  const digest = createHmac("sha1", decodeBase32(secretBase32)).update(message).digest();
  const offset = digest[digest.length - 1] & 0x0f;
  const binary =
    ((digest[offset] & 0x7f) << 24) |
    ((digest[offset + 1] & 0xff) << 16) |
    ((digest[offset + 2] & 0xff) << 8) |
    (digest[offset + 3] & 0xff);
  return (binary % 1_000_000).toString().padStart(6, "0");
}

function wrongTotpCode(secretBase32: string): string {
  const now = Math.floor(Date.now() / 1000);
  const boundarySafeWindow = new Set(
    [-2, -1, 0, 1, 2].map((stepOffset) =>
      totpCode(secretBase32, now + stepOffset * 30),
    ),
  );
  for (let candidate = 0; candidate < 1_000_000; candidate += 1) {
    const code = candidate.toString().padStart(6, "0");
    if (!boundarySafeWindow.has(code)) return code;
  }
  throw new Error("could not select a TOTP code outside the accepted window");
}

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required; run the auth_ui case in crates/zeroship-auth/tests/`);
  }
  return value;
}

async function postFormInBrowser<T>(
  page: Page,
  path: string,
  fields: Record<string, string>,
): Promise<{ body: T; status: number }> {
  return page.evaluate(
    async ({ fields: submittedFields, url }) => {
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams(submittedFields),
      });
      return { body: (await response.json()) as T, status: response.status };
    },
    { fields, url: authUrl(path) },
  );
}

test(TOTP_TITLE, async ({ page }) => {
  const { email } = await createVerifiedSession(page, "TOTP");

  await test.step("enroll and confirm TOTP through the authenticated endpoints", async () => {
    const logoutGet = await page.goto(authUrl("/logout"));
    expect(logoutGet?.status(), "logout GET status before TOTP enrollment").toBe(200);
    const logoutForm = page.locator('form[action="/logout"]');
    const csrf = await logoutForm.locator('input[name="csrf"]').inputValue();

    const enrollResponse = await postFormInBrowser<{
      confirmed?: boolean;
      otpauth_uri?: string;
      secret?: string;
    }>(page, "/me/2fa/enroll", { csrf });
    expect(enrollResponse.status, "TOTP enrollment status").toBe(200);
    const enrollment = enrollResponse.body;
    expect(enrollment.confirmed, "new TOTP enrollment is pending").toBe(false);
    expect(enrollment.otpauth_uri, "TOTP provisioning URI").toMatch(/^otpauth:\/\/totp\//);
    expect(enrollment.secret, "TOTP enrollment secret").toMatch(/^[A-Z2-7]+$/);

    const secret = enrollment.secret as string;
    const confirmResponse = await postFormInBrowser<{ confirmed?: boolean }>(
      page,
      "/me/2fa/confirm",
      { csrf, code: totpCode(secret) },
    );
    expect(confirmResponse.status, "TOTP confirmation status").toBe(200);
    const confirmation = confirmResponse.body;
    expect(confirmation.confirmed, "TOTP confirmation body").toBe(true);
    console.log(
      `TOTP_EVIDENCE enrollment ${JSON.stringify({ enrollStatus: enrollResponse.status, confirmStatus: confirmResponse.status, confirmed: confirmation.confirmed })}`,
    );

    const logoutResponsePromise = waitForHttpResponse(page, "POST", "/logout");
    await logoutForm.getByRole("button", { name: "Sign out" }).click();
    const logoutResponse = await logoutResponsePromise;
    expect(logoutResponse.status(), "logout POST status after TOTP enrollment").toBe(302);
    await expect(page, "logout destination after TOTP enrollment").toHaveURL(
      (url) => url.pathname === "/login",
    );

    const passwordForm = page.locator('form[action="/login"]');
    await passwordForm.locator('input[name="email"]').fill(email);
    await passwordForm.locator('input[name="password"]').fill(PASSWORD);
    const passwordResponsePromise = waitForHttpResponse(page, "POST", "/login");
    await passwordForm.getByRole("button", { name: "Sign in" }).click();
    const passwordResponse = await passwordResponsePromise;
    expect(passwordResponse.status(), "password factor status").toBe(200);

    const challengeForm = page.locator('form[action="/login/2fa"]');
    await expect(challengeForm, "clean TOTP challenge form").toBeVisible();
    const codeInput = challengeForm.locator('input[name="code"]');
    const cleanEvidence = {
      status: passwordResponse.status(),
      url: page.url(),
      alertCount: await page.getByRole("alert").count(),
      ariaInvalid: await codeInput.getAttribute("aria-invalid"),
      ariaDescribedBy: await codeInput.getAttribute("aria-describedby"),
    };
    console.log(`TOTP_EVIDENCE clean_challenge ${JSON.stringify(cleanEvidence)}`);
    expect(cleanEvidence.alertCount, "clean TOTP challenge alert count").toBe(0);
    expect(cleanEvidence.ariaInvalid, "clean TOTP challenge aria-invalid").toBeNull();
    expect(cleanEvidence.ariaDescribedBy, "clean TOTP challenge aria-describedby").toBeNull();

    await codeInput.fill(wrongTotpCode(secret));
    const wrongResponsePromise = waitForHttpResponse(page, "POST", "/login/2fa");
    await challengeForm.getByRole("button", { name: "Verify" }).click();
    const wrongResponse = await wrongResponsePromise;
    expect(wrongResponse.status(), "wrong TOTP status").toBe(401);

    const error = page.getByRole("alert");
    await expect(error, "wrong TOTP alert").toBeVisible();
    await expect(error, "wrong TOTP message").toHaveText("invalid code");
    await expect(error, "wrong TOTP error id").toHaveAttribute("id", "form-error");
    const rejectedCodeInput = page.locator('form[action="/login/2fa"] input[name="code"]');
    await expect(rejectedCodeInput, "wrong TOTP aria-invalid").toHaveAttribute(
      "aria-invalid",
      "true",
    );
    await expect(rejectedCodeInput, "wrong TOTP aria-describedby").toHaveAttribute(
      "aria-describedby",
      "form-error",
    );
    await expect(page.locator("#form-error"), "wrong TOTP description target").toBeVisible();
    const wrongEvidence = {
      status: wrongResponse.status(),
      error: await error.innerText(),
      role: await error.getAttribute("role"),
      errorId: await error.getAttribute("id"),
      ariaInvalid: await rejectedCodeInput.getAttribute("aria-invalid"),
      ariaDescribedBy: await rejectedCodeInput.getAttribute("aria-describedby"),
      descriptionTargetCount: await page.locator("#form-error").count(),
    };
    console.log(`TOTP_EVIDENCE rejected_code ${JSON.stringify(wrongEvidence)}`);

    await rejectedCodeInput.fill(totpCode(secret));
    const correctResponsePromise = waitForHttpResponse(page, "POST", "/login/2fa");
    await page
      .locator('form[action="/login/2fa"]')
      .getByRole("button", { name: "Verify" })
      .click();
    const correctResponse = await correctResponsePromise;
    expect(correctResponse.status(), "correct TOTP status").toBe(303);
    await expect(page, "correct TOTP destination").toHaveURL((url) => url.pathname === "/me");
    await expect(page.getByRole("heading", { name: "Your account", exact: true })).toBeVisible();
    console.log(
      `TOTP_EVIDENCE accepted_code ${JSON.stringify({ status: correctResponse.status(), destination: new URL(page.url()).pathname })}`,
    );
  });
});

test(CONSENT_TITLE, async ({ page }) => {
  await createVerifiedSession(page, "Consent");
  const clientId = requiredEnv("ZEROSHIP_AUTH_UI_OIDC_CLIENT_ID");
  const redirectUri = requiredEnv("ZEROSHIP_AUTH_UI_OIDC_REDIRECT_URI");
  const state = `authui-consent-${randomUUID()}`;
  const verifier = randomBytes(32).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  const requestedScopes = [
    {
      id: "openid",
      label: "Verify your identity",
      description: null,
    },
    {
      id: "read:notes",
      label: "Read notes",
      description: "Read your notes",
    },
  ] as const;
  const query = new URLSearchParams({
    client_id: clientId,
    response_type: "code",
    scope: requestedScopes.map((scope) => scope.id).join(" "),
    redirect_uri: redirectUri,
    state,
    code_challenge: challenge,
    code_challenge_method: "S256",
    nonce: `authui-nonce-${randomUUID()}`,
  });

  const authorizeResponsePromise = waitForHttpResponse(page, "GET", "/oauth2/authorize");
  const consentResponse = await page.goto(authUrl(`/oauth2/authorize?${query.toString()}`));
  const authorizeResponse = await authorizeResponsePromise;
  expect(authorizeResponse.status(), "authorize redirect status").toBe(303);
  expect(consentResponse?.status(), "consent GET status").toBe(200);
  await expect(page, "consent URL").toHaveURL((url) => url.pathname === "/consent");

  const scopeRows = page.locator("ul.scopes > li");
  await expect(scopeRows, "requested scope row count").toHaveCount(requestedScopes.length);
  const scopeEvidence = [];
  for (const [index, scope] of requestedScopes.entries()) {
    const row = scopeRows.nth(index);
    await expect(row.getByText(scope.label, { exact: true }), `${scope.id} label`).toBeVisible();
    const description = row.locator(".scope-desc");
    if (scope.description === null) {
      scopeEvidence.push({ ...scope, descriptionCount: await description.count() });
      continue;
    }
    await expect(description, `${scope.id} description`).toBeVisible();
    await expect(description, `${scope.id} description text`).toHaveText(scope.description);
    const display = await description.evaluate((element) => getComputedStyle(element).display);
    expect(display, `${scope.id} description display`).toBe("block");
    scopeEvidence.push({ ...scope, display });
  }

  const allow = page.getByRole("button", { name: "Allow", exact: true });
  const deny = page.getByRole("button", { name: "Deny", exact: true });
  await expect(allow).toBeVisible();
  await expect(deny).toBeVisible();
  const allowBackground = await allow.evaluate((element) => getComputedStyle(element).backgroundColor);
  const denyBackground = await deny.evaluate((element) => getComputedStyle(element).backgroundColor);
  expect(
    allowBackground,
    `consent action backgrounds: Allow=${allowBackground} Deny=${denyBackground}`,
  ).not.toBe(denyBackground);
  console.log(
    `CONSENT_EVIDENCE render ${JSON.stringify({ authorizeStatus: authorizeResponse.status(), consentStatus: consentResponse?.status(), scopes: scopeEvidence, allowBackground, denyBackground })}`,
  );

  let callbackRequestUrl: string | undefined;
  await page.route(`${redirectUri}**`, async (route) => {
    callbackRequestUrl = route.request().url();
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      body: "<!doctype html><title>RP callback</title><h1>RP callback</h1>",
    });
  });
  const denyResponsePromise = waitForHttpResponse(page, "POST", "/consent/deny");
  const callbackResponsePromise = page.waitForResponse((response) => {
    const url = new URL(response.url());
    const expected = new URL(redirectUri);
    return url.origin === expected.origin && url.pathname === expected.pathname;
  });
  await deny.click();
  const [denyResponse, callbackResponse] = await Promise.all([
    denyResponsePromise,
    callbackResponsePromise,
  ]);
  expect(denyResponse.status(), "consent denial status").toBe(303);
  expect(callbackResponse.status(), "RP callback status").toBe(200);
  await expect(page, "consent denial RP destination").toHaveURL((url) => {
    const expected = new URL(redirectUri);
    return url.origin === expected.origin && url.pathname === expected.pathname;
  });
  const callback = new URL(page.url());
  expect(callback.searchParams.get("error"), "consent denial error").toBe("access_denied");
  expect(callback.searchParams.get("state"), "consent denial state").toBe(state);
  expect(callback.searchParams.get("code"), "consent denial authorization code").toBeNull();
  console.log(
    `CONSENT_EVIDENCE denial ${JSON.stringify({ denyStatus: denyResponse.status(), callbackStatus: callbackResponse.status(), callbackUrl: callbackRequestUrl, error: callback.searchParams.get("error"), state: callback.searchParams.get("state"), code: callback.searchParams.get("code") })}`,
  );

  const secondAuthorizeResponsePromise = waitForHttpResponse(
    page,
    "GET",
    "/oauth2/authorize",
  );
  const secondConsentResponse = await page.goto(
    authUrl(`/oauth2/authorize?${query.toString()}`),
  );
  const secondAuthorizeResponse = await secondAuthorizeResponsePromise;
  expect(secondAuthorizeResponse.status(), "authorize status after consent denial").toBe(303);
  expect(secondConsentResponse?.status(), "consent status after denial").toBe(200);
  await expect(page, "denied consent was not persisted as a grant").toHaveURL(
    (url) => url.pathname === "/consent",
  );
  console.log(
    `CONSENT_EVIDENCE denial_not_grant ${JSON.stringify({ authorizeStatus: secondAuthorizeResponse.status(), consentStatus: secondConsentResponse?.status(), destination: new URL(page.url()).pathname })}`,
  );
});
