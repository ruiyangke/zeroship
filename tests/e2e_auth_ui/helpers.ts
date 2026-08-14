import { readFile } from "node:fs/promises";

export interface CspViolationEvidence {
  blockedURI: string;
  disposition: string;
  effectiveDirective: string;
  originalPolicy: string;
  sourceFile: string;
  violatedDirective: string;
}

export function authUrl(path: string): string {
  const rawBaseURL = process.env.ZEROSHIP_AUTH_UI_BASE_URL;
  if (!rawBaseURL) {
    throw new Error("ZEROSHIP_AUTH_UI_BASE_URL is required; use tests/e2e_auth_ui.sh");
  }
  return new URL(path, rawBaseURL).href;
}

export function authLogPath(): string {
  const path = process.env.ZEROSHIP_AUTH_UI_AUTH_LOG;
  if (!path) {
    throw new Error("ZEROSHIP_AUTH_UI_AUTH_LOG is required; use tests/e2e_auth_ui.sh");
  }
  return path;
}

function verificationUrlFromLog(log: string, recipient: string): string | undefined {
  const expectedOrigin = new URL(authUrl("/")).origin;
  const completedMail = /=== MAIL ===\r?\n([\s\S]*?)\r?\n=== END ===/g;

  for (const match of log.matchAll(completedMail)) {
    const lines = match[1].split(/\r?\n/).map((line) => line.trim());
    if (!lines.includes(`RCPT TO (envelope): ${recipient}`)) continue;
    if (!lines.includes("Subject: Verify your zeroship email")) continue;

    for (const line of lines) {
      let candidate: URL;
      try {
        candidate = new URL(line);
      } catch {
        continue;
      }
      const token = candidate.searchParams.get("token") ?? "";
      if (
        candidate.origin === expectedOrigin &&
        candidate.pathname === "/verify" &&
        /^[A-Za-z0-9_-]+$/.test(token)
      ) {
        return candidate.href;
      }
    }
  }

  return undefined;
}

export async function waitForVerificationUrl(
  recipient: string,
  timeoutMs = 15_000,
): Promise<string> {
  const path = authLogPath();
  const deadline = Date.now() + timeoutMs;

  do {
    const verificationUrl = verificationUrlFromLog(await readFile(path, "utf8"), recipient);
    if (verificationUrl) return verificationUrl;
    await new Promise((resolve) => setTimeout(resolve, 100));
  } while (Date.now() < deadline);

  throw new Error(
    `verification email for ${JSON.stringify(recipient)} did not appear in ${path} within ${timeoutMs}ms`,
  );
}
