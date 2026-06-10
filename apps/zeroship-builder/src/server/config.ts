import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    // No `/auth/*` resource: the console no longer serves a bespoke OAuth
    // RP. End-user auth runs through the platform BFF — the gateway's
    // same-origin `/__zeroship/auth/*` endpoints (served by the gateway, not the
    // app) + the `@zeroship/auth` SDK. Identity reaches app code via the
    // gateway-forwarded `ZeroShip-User` header (`currentUser()`).
    "/api/preview": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
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
    // the gateway's Anon default with no rate limit. The auto-derived
    // per-procedure resources (`rpc:projects.<name>`) inherit auth +
    // rateLimit from this `rpc:projects` ancestor.
    "rpc:projects": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
    },
    "rpc:sandbox": {
      auth: "user",
      rateLimit: { rpm: 300, per: "user" },
    },
    "rpc:agents": {
      auth: "user",
      rateLimit: { rpm: 300, per: "user" },
    },
    "rpc:chat": {
      auth: "user",
      rateLimit: { rpm: 60, per: "user" },
      maxInputBytes: 262_144,
    },
    "rpc:wizard": {
      auth: "anon",
      publiclyAccessible: true,
      rateLimit: { rpm: 20, per: "ip" },
      maxInputBytes: 65_536,
    },
    "rpc:pm": {
      auth: "admin",
      rateLimit: { rpm: 60, per: "app" },
    },
    "rpc:sre": {
      auth: "admin",
      rateLimit: { rpm: 60, per: "app" },
    },
  },
});
