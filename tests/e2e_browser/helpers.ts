import { readFileSync } from "node:fs";
import { DESCRIPTOR, type AppKind, type StackDescriptor } from "./fixture/descriptor";

let cached: StackDescriptor | null = null;

function stack(): StackDescriptor {
  cached ??= JSON.parse(readFileSync(DESCRIPTOR, "utf8")) as StackDescriptor;
  return cached;
}

/**
 * Browser-facing URL for a deployed app. `<slug>.localhost` resolves to
 * 127.0.0.1 in every modern browser, and the gateway takes the app name from
 * the first Host label after stripping `:port`
 * (crates/zeroship-gateway/src/router/dispatch.rs::extract_app_name).
 */
export function appUrl(kind: AppKind, path = "/"): string {
  const s = stack();
  const slug = s.apps[kind];
  if (!slug) throw new Error(`the stack descriptor ${DESCRIPTOR} names no '${kind}' app`);
  const p = path.startsWith("/") ? path : `/${path}`;
  return `http://${slug}.localhost:${s.gatePort}${p}`;
}
