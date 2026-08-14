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
