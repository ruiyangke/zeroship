import { defineApp } from "@zeroship/server";

// Every procedure is anonymously reachable ON PURPOSE.
//
// The platform default is fail-closed (`auth: "user"`), and a gated procedure is
// answered by the GATEWAY before dispatch - so its error body is the gateway's,
// never the worker's. `tests/e2e_dev_vs_deployed_errors.sh` exists to measure
// the WORKER's error envelope, which is only observable when the call actually
// reaches the worker. Declaring `auth: "anon"` here is what makes the
// measurement possible; adding a gate would silently delete it.
export default defineApp({
  resources: {
    "rpc:err.plain": { auth: "anon", publiclyAccessible: true },
    "rpc:err.status4xx": { auth: "anon", publiclyAccessible: true },
    "rpc:err.status4xxCode": { auth: "anon", publiclyAccessible: true },
    "rpc:err.publicCode5xx": { auth: "anon", publiclyAccessible: true },
    "rpc:err.ok": { auth: "anon", publiclyAccessible: true },
  },
});
