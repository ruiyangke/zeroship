import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DESCRIPTOR = join(__dirname, ".stack.json");

export type AppKind = "csr" | "ssr" | "ssg";

export interface StackDescriptor {
  gatePort: number;
  controlPort: number;
  workerPort: number;
  work: string;
  pidfile: string;
  pgContainer: string;
  apps: Record<AppKind, string | null>;
  appIds: Record<AppKind, string | null>;
}

let cached: StackDescriptor | null = null;

export function stack(): StackDescriptor {
  if (cached) return cached;
  const raw = readFileSync(DESCRIPTOR, "utf8");
  cached = JSON.parse(raw) as StackDescriptor;
  return cached;
}

/** Slug for an app kind, or null if it wasn't deployed (dist missing). */
export function appSlug(kind: AppKind): string | null {
  return stack().apps[kind];
}

/**
 * Browser-facing URL for a deployed app. `<slug>.localhost` resolves to
 * 127.0.0.1 in every modern browser, and the gateway extracts the slug from
 * the first Host subdomain after stripping `:port`
 * (crates/gateway/src/router/dispatch.rs::extract_app_name).
 */
export function appUrl(kind: AppKind, path = "/"): string {
  const s = stack();
  const slug = s.apps[kind];
  if (!slug) throw new Error(`app kind '${kind}' was not deployed (no slug in descriptor)`);
  const p = path.startsWith("/") ? path : `/${path}`;
  return `http://${slug}.localhost:${s.gatePort}${p}`;
}
