import { defineApp } from "@zeroship/server";

// Every procedure is anonymously reachable ON PURPOSE.
//
// The platform default is fail-closed (`auth: "user"`), and a gated procedure is
// answered by the GATEWAY before dispatch - so its error body is the gateway's,
// never the worker's. `tests/e2e_dev_vs_deployed_errors.sh` exists to measure
// the WORKER's error envelope, which is only observable when the call actually
// reaches the worker. Declaring `auth: "anonymous"` here is what makes the
// measurement possible; adding a gate would silently delete it.
export default defineApp({
  resources: {
    "rpc:err.plain": { auth: "anonymous", publiclyAccessible: true },
    "rpc:err.status4xx": { auth: "anonymous", publiclyAccessible: true },
    "rpc:err.status4xxCode": { auth: "anonymous", publiclyAccessible: true },
    "rpc:err.publicCode5xx": { auth: "anonymous", publiclyAccessible: true },
    "rpc:err.ok": { auth: "anonymous", publiclyAccessible: true },
    // The dispatcher leg. Anon for the same reason as the rest: a gated
    // procedure is answered by the gateway's auth gate BEFORE dispatch, so an
    // INVALID_ARGUMENT that the dispatcher would have produced never happens
    // and the row would read as a clean 401 rather than as "never measured".
    "rpc:err.needsInput": { auth: "anonymous", publiclyAccessible: true },
  },
});
