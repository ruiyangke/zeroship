// Server-side env access. The zeroship runtime exposes user-set vars
// via the `env` import; these helpers normalize defaults so the
// rest of the server code reads from one place.

// In production these come from `zeroship secret set`. In dev, the
// zeroship vite-plugin reads from the host's process env.

declare const env: {
  CONTROL_URL?: string;
  CONTROL_KEY?: string;
  SANDBOX_URL?: string;
  SANDBOX_TOKEN?: string;
  OPENAI_API_KEY?: string;
};

function readEnv(key: string, fallback: string): string {
  // Zeroship `env` is a frozen object on globalThis; we read from
  // it dynamically because the type declaration above is only for
  // editor support.
  const e = (globalThis as any).env;
  if (e && typeof e[key] === "string" && e[key]) return e[key];
  return fallback;
}

export const CONTROL_URL  = () => readEnv("CONTROL_URL",  "http://localhost:9090");
export const CONTROL_KEY  = () => readEnv("CONTROL_KEY",  "dev-master-key");
export const SANDBOX_URL  = () => readEnv("SANDBOX_URL",  "http://localhost:9091");
export const SANDBOX_TOKEN = () => readEnv("SANDBOX_TOKEN", "test");
export const OPENAI_API_KEY = () => readEnv("OPENAI_API_KEY", "");
