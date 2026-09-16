import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";

import {
  expect,
  test,
  type BrowserContext,
  type Page,
  type Response,
} from "@playwright/test";
import { authUrl, waitForVerificationUrl } from "../helpers";

const SESSION_COOKIE = "__Host-zsidp_session";
const JOURNEY_TITLE =
  "creator journey covers signup, verification, login, profile, logout, and defenses";

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

async function sessionCookie(context: BrowserContext) {
  // Chromium accepts Secure cookies on trustworthy loopback origins, but its
  // URL-filtered cookie API omits them for an http URL. Inspect the full jar.
  return (await context.cookies()).find((cookie) => cookie.name === SESSION_COOKIE);
}

async function redeemVerificationLink(page: Page, verificationUrl: string): Promise<Response> {
  const redeemResponsePromise = waitForHttpResponse(page, "POST", "/verify/redeem");
  await page.goto(verificationUrl, { waitUntil: "commit" });
  const response = await redeemResponsePromise;
  await page.waitForLoadState("domcontentloaded");
  return response;
}

interface FailedLoginEvidence {
  message: string;
  status: number;
}

async function submitFailedLogin(
  page: Page,
  email: string,
  password: string,
): Promise<FailedLoginEvidence> {
  const loginGet = await page.goto(authUrl("/login"));
  expect(loginGet?.status(), "failed-login setup GET status").toBe(200);

  const form = page.locator('form[action="/login"]');
  await form.locator('input[name="email"]').fill(email);
  await form.locator('input[name="password"]').fill(password);
  const loginResponsePromise = waitForHttpResponse(page, "POST", "/login");
  await form.getByRole("button", { name: "Sign in" }).click();
  const response = await loginResponsePromise;
  const error = page.locator(".error");
  await expect(error, `failed login error for ${JSON.stringify(email)}`).toBeVisible();

  return {
    message: await error.innerText(),
    status: response.status(),
  };
}

test(JOURNEY_TITLE, async ({ context, page }) => {
  const id = randomUUID();
  const name = `Browser Creator ${id.slice(0, 8)}`;
  const email = `authui-${id}@example.test`;
  const password = "correct horse battery staple 2026!";
  let verificationUrl = "";

  await test.step("1. sign up", async () => {
    const signupGet = await page.goto(authUrl("/signup"));
    expect(signupGet?.status(), "signup GET status").toBe(200);
    await expect(page, "signup URL").toHaveURL(authUrl("/signup"));

    const form = page.locator('form[action="/signup"]');
    await form.locator('input[name="name"]').fill(name);
    await form.locator('input[name="email"]').fill(email);
    await form.locator('input[name="password"]').fill(password);
    const signupResponsePromise = waitForHttpResponse(page, "POST", "/signup");
    await form.getByRole("button", { name: "Sign up" }).click();
    const response = await signupResponsePromise;

    expect(response.status(), "signup POST status").toBe(302);
    await expect(page, "successful signup destination").toHaveURL(
      (url) => url.pathname === "/login" && url.searchParams.get("return_to") === "/me",
    );
    console.log(`JOURNEY_EVIDENCE signup ${JSON.stringify({ status: response.status() })}`);
  });

  await test.step("2. verify the email from the stdout mail", async () => {
    verificationUrl = await waitForVerificationUrl(email);
    const parsedVerificationUrl = new URL(verificationUrl);
    expect(parsedVerificationUrl.origin, "verification link origin").toBe(
      new URL(authUrl("/")).origin,
    );
    expect(parsedVerificationUrl.pathname, "verification link path").toBe("/verify");
    expect(parsedVerificationUrl.searchParams.get("token"), "verification link token").toMatch(
      /^[A-Za-z0-9_-]+$/,
    );

    const response = await redeemVerificationLink(page, verificationUrl);
    expect(response.status(), "verification redeem status").toBe(200);
    await expect(page, "verification result URL").toHaveURL(
      (url) => url.pathname === "/verify/redeem",
    );
    await expect(page.getByRole("heading", { name: "Email verified", exact: true })).toBeVisible();
    await expect(page.getByText(email, { exact: true })).toBeVisible();
    console.log(`JOURNEY_EVIDENCE verification ${JSON.stringify({ status: response.status() })}`);
  });

  await test.step("3. sign in and receive a session cookie", async () => {
    const loginGet = await page.goto(authUrl("/login"));
    expect(loginGet?.status(), "login GET status").toBe(200);
    expect(await sessionCookie(context), "session cookie before login").toBeUndefined();

    const form = page.locator('form[action="/login"]');
    await form.locator('input[name="email"]').fill(email);
    await form.locator('input[name="password"]').fill(password);
    const loginResponsePromise = waitForHttpResponse(page, "POST", "/login");
    await form.getByRole("button", { name: "Sign in" }).click();
    const response = await loginResponsePromise;

    expect(response.status(), "successful login POST status").toBe(303);
    await expect(page, "successful login destination").toHaveURL(
      (url) => url.pathname === "/me",
    );
    const cookie = await sessionCookie(context);
    expect(cookie, `${SESSION_COOKIE} after login`).toBeDefined();
    expect(cookie?.value, `${SESSION_COOKIE} value after login`).not.toBe("");
    console.log(`JOURNEY_EVIDENCE login ${JSON.stringify({ status: response.status(), cookieSet: true })}`);
  });

  await test.step("4. view the signed-in profile", async () => {
    const response = await page.goto(authUrl("/me"));
    expect(response?.status(), "authenticated /me status").toBe(200);
    await expect(page, "profile URL").toHaveURL(authUrl("/me"));
    await expect(page.getByRole("heading", { name: "Your account", exact: true })).toBeVisible();
    await expect(page.locator("dl.profile").getByText(email, { exact: true })).toBeVisible();
    await expect(page.locator("dl.profile").getByText(name, { exact: true })).toBeVisible();
    console.log(`JOURNEY_EVIDENCE profile ${JSON.stringify({ status: response?.status(), email, name })}`);
  });

  await test.step("5. log out and lose access to the profile", async () => {
    const logoutGet = await page.goto(authUrl("/logout"));
    expect(logoutGet?.status(), "logout GET status").toBe(200);
    const form = page.locator('form[action="/logout"]');
    const logoutResponsePromise = waitForHttpResponse(page, "POST", "/logout");
    await form.getByRole("button", { name: "Sign out" }).click();
    const response = await logoutResponsePromise;

    expect(response.status(), "logout POST status").toBe(302);
    await expect(page, "logout destination").toHaveURL((url) => url.pathname === "/login");
    expect(await sessionCookie(context), "session cookie after logout").toBeUndefined();

    const meResponsePromise = waitForHttpResponse(page, "GET", "/me");
    await page.goto(authUrl("/me"));
    const meResponse = await meResponsePromise;
    expect(meResponse.status(), "logged-out /me status").toBe(302);
    await expect(page, "logged-out /me destination").toHaveURL(
      (url) => url.pathname === "/login",
    );
    await expect(page.locator("body"), "logged-out page hides the account email").not.toContainText(
      email,
    );
    await expect(page.locator("body"), "logged-out page hides the account name").not.toContainText(
      name,
    );
    console.log(
      `JOURNEY_EVIDENCE logout ${JSON.stringify({ status: response.status(), meStatus: meResponse.status(), cookieCleared: true })}`,
    );
  });

  await test.step("6. equalize wrong-password and missing-email failures", async () => {
    const wrongPassword = await submitFailedLogin(page, email, `${password}-wrong`);
    const missingEmail = await submitFailedLogin(
      page,
      `missing-${randomUUID()}@example.test`,
      `${password}-wrong`,
    );

    console.log(`JOURNEY_EVIDENCE enumeration ${JSON.stringify({ wrongPassword, missingEmail })}`);
    expect(wrongPassword.status, "right email with wrong password status").toBe(401);
    expect(missingEmail.status, "missing email status").toBe(401);
    expect(wrongPassword.message, "right email with wrong password error text").not.toBe("");
    expect(missingEmail.message, "missing email error text").not.toBe("");
    expect(
      Buffer.from(missingEmail.message, "utf8"),
      `login errors differ: wrong-password=${JSON.stringify(wrongPassword.message)} missing-email=${JSON.stringify(missingEmail.message)}`,
    ).toEqual(Buffer.from(wrongPassword.message, "utf8"));
  });

  await test.step("7. reject a second use of the verification link", async () => {
    const response = await redeemVerificationLink(page, verificationUrl);
    await expect(page.getByRole("heading", { name: "session expired", exact: true })).toBeVisible();
    await expect(
      page.getByRole("heading", { name: "Email verified", exact: true }),
      "verification replay must not render success",
    ).toHaveCount(0);
    await expect(page.getByText("Error code: session_expired", { exact: true })).toBeVisible();
    console.log(`JOURNEY_EVIDENCE verification_replay ${JSON.stringify({ status: response.status() })}`);
  });
});
