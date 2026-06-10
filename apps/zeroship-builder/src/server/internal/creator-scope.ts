"use server";
// Acting-creator resolution + creator-scoped KV keys (SEC-10).
//
// The builder console is ONE shared zeroship app: every creator shares
// its single `@zeroship/kv` namespace, and `appId` is a client-supplied
// arbitrary string. Any per-app KV slot must therefore be namespaced by
// the ACTING creator, never keyed on `appId` alone — otherwise reading
// another creator's data is a one-string IDOR. `projects.ts` pioneered
// the pattern (`projects:${creatorId}`); the issue-tracker and
// quality-scorecard keys here follow it.
//
// Identity contract (mirrors `resolveSandboxUserId`, SEC-6):
//   - authenticated → the platform subject from `currentUser()` (the
//     gateway-verified `ZeroShip-User` envelope; a `pws_…` pairwise in
//     prod, the dev-auth identity under `pnpm dev`);
//   - unauthenticated + `ZEROSHIP_DEV=1` → the single stable dev id, so
//     local tools see one coherent dev workspace;
//   - unauthenticated otherwise → throw. Fail closed: these procedures
//     are user-auth at the gateway (config.ts), so this branch firing
//     in prod means an auth regression — refuse rather than silently
//     scope to a shared fallback.
//
// Naming: underscore-internal module — `server.ts` does not re-export
// it, so nothing here becomes a public RPC endpoint.

import { currentUser } from "zeroship";
import { DEV_SANDBOX_USER_ID, isLocalDevRuntime } from "./sandbox-backend.js";

/** The acting creator's stable id. The KV namespaces below are scoped
 *  to it so different creators can never read or clobber each other's
 *  slots. */
export function currentCreatorId(): string {
  try {
    const user = currentUser() as { id?: unknown } | null;
    if (typeof user?.id === "string" && user.id.length > 0) return user.id;
  } catch {
    // currentUser() throws outside a request handler (dev/test).
  }
  if (isLocalDevRuntime()) {
    // Single-sourced with the sandbox's dev owner so a dev creator's
    // registries and their sandboxes share one identity.
    return DEV_SANDBOX_USER_ID;
  }
  throw new Error("creator identity unavailable: request is not authenticated");
}

/** Issue-tracker slot for one of the acting creator's projects. */
export function issuesKey(appId: string): string {
  return `issues:${currentCreatorId()}:${appId}`;
}

/** Quality-scorecard slot for one of the acting creator's projects. */
export function qualityKey(appId: string): string {
  return `quality:${currentCreatorId()}:${appId}`;
}
