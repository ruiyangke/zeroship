import { defineApp } from "@zeroship/server";

// Auth postures for the dev-vs-deployed auth comparison.
//
// The platform default is fail-closed: a procedure with NO entry here resolves
// to `auth: "user"`, so forgetting the file is a loud 401 once deployed rather
// than a silent public endpoint.
//
// `probe.defaulted` is INTENTIONALLY MISSING from this map, and
// `probe.requireGated` is here only to pair with it. Between them they hold the
// #163 surface: the fail-closed default is enforced by the gateway
// (`crates/zeroship-gateway/src/router/dispatch.rs`), and `pnpm dev` has no gateway, so
// the alarm the default's safety argument depends on is inaudible for the whole
// local development cycle. Adding `probe.defaulted` here would delete the
// measurement, not fix anything. See docs/pilot/e2e-scenarios.md scenario 6.
export default defineApp({
  resources: {
    "rpc:probe.public": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.userDeclared": { auth: "user" },
    // Anonymously reachable ON PURPOSE: it is how the kernel's own
    // `requireUser()` throw becomes observable on the deployed side, where a
    // gated procedure is answered by the gateway and never reaches the worker.
    "rpc:probe.requireAnon": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.requireGated": { auth: "user" },
    "rpc:probe.appGate": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.userShape": { auth: "anonymous", publiclyAccessible: true },
  },
});
