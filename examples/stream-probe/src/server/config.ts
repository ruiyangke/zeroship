import { defineApp } from "@zeroship/server";

// Both procedures are opted into anonymous access. Without this every RPC
// resolves to `auth: "user"` and the gateway refuses it, which is green under
// pnpm dev and 401 deployed -- the trap examples/kv-dashboard shipped with.
// This app has no users and no per-user data; it emits a fixed counter.
export default defineApp({
  resources: {
    "rpc:probe.ticks": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.ping": { auth: "anonymous", publiclyAccessible: true },
  },
});
