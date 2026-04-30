// ─── e2e helpers — real RPC, no mocks ────────────────────────────
//
// The zeroship-builder dev server proxies every server-function call
// to a real zeroship runtime at :3001 which itself proxies to the
// real control plane on :9090. These helpers call the same RPC
// endpoints the app uses, so test fixtures hit real state.

import type { APIRequestContext, Page } from "@playwright/test";

export const APP_URL = "http://localhost:5173";
export const RPC_URL = "http://localhost:3001";

export interface AppRecord {
  id: string;
  name: string;
  plan_id: string;
  deploy_hash: string | null;
  created_at: string;
  updated_at: string;
}

const TOKEN_ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789";
function randomToken(n = 6): string {
  let out = "";
  for (let i = 0; i < n; i++) out += TOKEN_ALPHABET[Math.floor(Math.random() * TOKEN_ALPHABET.length)];
  return out;
}

/** Test-app slugs always start with `e2e-` so cleanup can find leaks. */
export function uniqueAppName(prefix = "e2e"): string {
  return `${prefix}-${randomToken(6)}`;
}

/** Call a server-function RPC by its registered name. */
export async function rpc<T = any>(
  request: APIRequestContext,
  method: string,
  args: any[] = [],
): Promise<T> {
  const r = await request.post(`${RPC_URL}/_rpc/${method}`, {
    data: args,
    headers: { "content-type": "application/json" },
  });
  if (!r.ok()) {
    throw new Error(`rpc ${method} → HTTP ${r.status()}: ${await r.text()}`);
  }
  return (await r.json()) as T;
}

// ─── App lifecycle ────────────────────────────────────────────────

export async function listApps(request: APIRequestContext): Promise<AppRecord[]> {
  return rpc(request, "src/server/apps/listApps");
}

export async function createApp(
  request: APIRequestContext,
  name?: string,
  plan: "free" | "pro" | "unlimited" = "free",
): Promise<AppRecord> {
  const slug = name ?? uniqueAppName();
  return rpc(request, "src/server/apps/createApp", [slug, plan]);
}

export async function deleteApp(
  request: APIRequestContext,
  id: string,
): Promise<void> {
  try {
    await rpc(request, "src/server/apps/deleteApp", [id]);
  } catch {
    // best-effort cleanup — never fail a test on a leaked fixture
  }
}

export async function listVars(
  request: APIRequestContext,
  appId: string,
): Promise<{ vars: { key: string; value: string }[] }> {
  return rpc(request, "src/server/apps/listVars", [appId]);
}

export async function listSecrets(
  request: APIRequestContext,
  appId: string,
): Promise<{ secrets: string[] }> {
  return rpc(request, "src/server/apps/listSecrets", [appId]);
}

/** Sweep all `e2e-*` apps that previous test runs may have leaked. */
export async function cleanupLeakedTestApps(
  request: APIRequestContext,
): Promise<number> {
  const apps = await listApps(request);
  const leaked = apps.filter((a) => a.name.startsWith("e2e-"));
  for (const a of leaked) await deleteApp(request, a.id);
  return leaked.length;
}

// ─── Page helpers ─────────────────────────────────────────────────

/** Visit a path and wait for React to finish first paint. */
export async function visit(page: Page, path: string): Promise<void> {
  await page.goto(path);
  // Wait for the BrowserRouter to settle — the AuthProvider's loading
  // state ends quickly in dev (devBypass returns synthetic user
  // synchronously), so just yielding to the event loop is enough.
  await page.waitForLoadState("networkidle").catch(() => {});
}
