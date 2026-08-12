import { defineApp } from "@zeroship/server";

// Anonymously reachable ON PURPOSE, for the same reason examples/env-probe is.
//
// The platform default is fail-closed (`auth: "user"`), and a gated procedure
// is answered by the GATEWAY's auth gate BEFORE dispatch - so it never reaches
// the worker, and the deployed row would describe the gateway's refusal rather
// than the worker's wall-clock enforcement. A 401 arrives fast and looks like a
// clean answer, which would hide the very thing this app measures.
export default defineApp({
  resources: {
    "rpc:wallp.fast": { auth: "anon", publiclyAccessible: true },
    "rpc:wallp.slow": { auth: "anon", publiclyAccessible: true },
  },
});
