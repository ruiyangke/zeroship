"use server";
// Server-side env access. The zeroship runtime exposes user-set vars
// via the `env` import; these helpers normalize defaults so the
// rest of the server code reads from one place.

// In production these come from `zeroship secret set`. In dev, the
// zeroship vite-plugin reads from the host's process env.

declare const env: {
  SANDBOX_URL?: string;
  SANDBOX_TOKEN?: string;
  ZEROSHIP_SDK_REGISTRY?: string;
  OPENAI_API_KEY?: string;
};

function readEnv(key: string, fallback: string): string {
  // The zeroship runtime exposes per-app vars + secrets via
  // `process.env.X` (init.rs `setup_globals`). Older revisions also
  // installed a `globalThis.env` mirror; check it as a secondary
  // source for forward-compat.
  const proc = (globalThis as any).process;
  if (proc?.env && typeof proc.env[key] === "string" && proc.env[key]) {
    return proc.env[key];
  }
  const e = (globalThis as any).env;
  if (e && typeof e[key] === "string" && e[key]) return e[key];
  return fallback;
}

export const SANDBOX_URL  = () => readEnv("SANDBOX_URL",  "http://localhost:9091");

export const SANDBOX_TOKEN = () => readEnv("SANDBOX_TOKEN", "test");

export const ZEROSHIP_SDK_REGISTRY = () => readEnv("ZEROSHIP_SDK_REGISTRY", "");

export const OPENAI_API_KEY = () => readEnv("OPENAI_API_KEY", "");
