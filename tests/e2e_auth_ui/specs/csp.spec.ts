import { expect, test } from "@playwright/test";
import { authUrl, type CspViolationEvidence } from "../helpers";

test("login applies its layout without style-src violations", async ({ page }) => {
  const consoleMessages: string[] = [];
  page.on("console", (message) => {
    consoleMessages.push(`[${message.type()}] ${message.text()}`);
  });

  await page.addInitScript(() => {
    const target = window as typeof window & {
      __zeroshipCspViolations?: CspViolationEvidence[];
    };
    target.__zeroshipCspViolations = [];
    document.addEventListener("securitypolicyviolation", (event) => {
      target.__zeroshipCspViolations?.push({
        blockedURI: event.blockedURI,
        disposition: event.disposition,
        effectiveDirective: event.effectiveDirective,
        originalPolicy: event.originalPolicy,
        sourceFile: event.sourceFile,
        violatedDirective: event.violatedDirective,
      });
    });
  });

  const response = await page.goto(authUrl("/login"), { waitUntil: "networkidle" });
  expect(response?.status()).toBe(200);
  const contentSecurityPolicy = response?.headers()["content-security-policy"] ?? "";
  const styleDirective = contentSecurityPolicy
    .split(";")
    .map((directive) => directive.trim())
    .find((directive) => directive.startsWith("style-src"));
  expect(styleDirective, "the enforced style-src directive").toBe("style-src 'self'");
  expect(contentSecurityPolicy).not.toContain("'unsafe-inline'");
  expect(contentSecurityPolicy).not.toContain("'unsafe-hashes'");

  const violations = await page.evaluate(() => {
    const target = window as typeof window & {
      __zeroshipCspViolations?: CspViolationEvidence[];
    };
    return target.__zeroshipCspViolations ?? [];
  });
  const styleEvents = violations.filter(
    (event) =>
      event.effectiveDirective.startsWith("style-src") ||
      event.violatedDirective.startsWith("style-src"),
  );
  const styleConsoleMessages = consoleMessages.filter(
    (message) => /content security policy/i.test(message) && /style-src/i.test(message),
  );
  const styleViolationEvidence = [
    ...styleConsoleMessages.map((message) => `console: ${message}`),
    ...styleEvents.map((event) => `event: ${JSON.stringify(event)}`),
  ];

  expect(
    styleViolationEvidence,
    `style-src violations:\n${styleViolationEvidence.join("\n")}`,
  ).toEqual([]);

  const forgotLinkRow = page.locator('p:has(a[href="/forgot"])');
  await expect(forgotLinkRow).toHaveCSS("text-align", "right");
});
