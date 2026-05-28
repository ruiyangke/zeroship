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
    },
    "/api/preview": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
    },
    "/api/preview/*": {
      auth: "user",
      rateLimit: { rpm: 600, per: "user" },
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
