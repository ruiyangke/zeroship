import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    // Fail-closed root (SEC-5): every resource that does not declare —
    // or inherit — an auth level resolves here instead of falling
    // through to the gateway's Anon default
    // (crates/gateway/src/compiled.rs `resolve_effective_policy`). A new
    // procedure whose wireId matches none of the family policies below
    // therefore ships authenticated, never silently public. Weakening
    // requires an explicit `auth: "anon"` + `publiclyAccessible: true`
    // + `override: ["auth"]` on the resource itself (see `rpc:wizard`).
    "*": {
      auth: "user",
    },
    // Every resource below redeclares `auth` already declared by the
    // root `*`, so each carries `override: ["auth"]` — the manifest
    // validator (crates/bundle `Manifest::validate`) rejects shadowed
    // fields without the explicit marker.
    //
    // No `/auth/*` resource: the console no longer serves a bespoke OAuth
    // RP. End-user auth runs through the platform BFF — the gateway's
    // same-origin `/__zeroship/auth/*` endpoints (served by the gateway, not the
    // app) + the `@zeroship/auth` SDK. Identity reaches app code via the
    // gateway-forwarded `ZeroShip-User` header (`currentUser()`).
    "/api/preview": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
      override: ["auth"],
    },
    "/api/preview/*": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
      // Redeclares auth + rateLimit from the "/api/preview" ancestor with the
      // SAME values; override confirms the intentional (redundant) shadow so the
      // manifest validator (crates/bundle Manifest::validate) accepts it.
      override: ["auth", "rate_limit"],
    },
    // SEC-5: this family policy MUST be keyed by the procedures' actual
    // wireId prefix. The procedures in projects.ts carry `projects.*`
    // ids; the key was `rpc:apps` (a relic of the module being apps.ts),
    // which matched nothing and left all ten procedures — including
    // getEnv/setEnv/getLogs over sandbox env + secrets — resolving to
    // Anon with no rate limit.
    "rpc:projects": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
      override: ["auth"],
    },
    "rpc:sandbox": {
      auth: "user",
      rateLimit: { rpm: 300, per: "user" },
      override: ["auth"],
    },
    "rpc:agents": {
      auth: "user",
      rateLimit: { rpm: 300, per: "user" },
      override: ["auth"],
    },
    "rpc:chat": {
      auth: "user",
      rateLimit: { rpm: 60, per: "user" },
      maxInputBytes: 262_144,
      override: ["auth"],
    },
    "rpc:wizard": {
      auth: "anon",
      publiclyAccessible: true,
      rateLimit: { rpm: 20, per: "ip" },
      maxInputBytes: 65_536,
      override: ["auth"],
    },
    "rpc:pm": {
      auth: "admin",
      rateLimit: { rpm: 60, per: "app" },
      override: ["auth"],
    },
    "rpc:sre": {
      auth: "admin",
      rateLimit: { rpm: 60, per: "app" },
      override: ["auth"],
    },
  },
});
