import { defineApp } from "@zeroship/server";

export default defineApp({
  resources: {
    "/auth": {
      auth: "anon",
      publiclyAccessible: true,
    },
    "/auth/*": {
      auth: "anon",
      publiclyAccessible: true,
      // Redeclares auth + publiclyAccessible from the "/auth" ancestor with the
      // SAME values; override confirms the intentional (redundant) shadow so the
      // manifest validator (crates/bundle Manifest::validate) accepts it.
      override: ["auth", "publicly_accessible"],
    },
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
    "rpc:apps": {
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
