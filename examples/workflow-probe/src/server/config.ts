import { defineApp } from "@zeroship/server";

// Every procedure is explicitly anonymous. Without this file each one resolves
// to `auth: "user"` and the gateway refuses it -- green under `pnpm dev`, 401
// deployed, which is how examples/kv-dashboard shipped. This app has no users
// and no per-user data; it starts fixed workflows and reads their status.
export default defineApp({
  resources: {
    "rpc:wf.ping": { auth: "anon", publiclyAccessible: true },
    "rpc:wf.start": { auth: "anon", publiclyAccessible: true },
    "rpc:wf.status": { auth: "anon", publiclyAccessible: true },
    "rpc:wf.signal": { auth: "anon", publiclyAccessible: true },
    "rpc:wf.trail": { auth: "anon", publiclyAccessible: true },
    "rpc:wf.resetTrail": { auth: "anon", publiclyAccessible: true },
  },
});
