import { defineApp } from "@zeroship/server";

// Every procedure is opted into anonymous access.
//
// The platform default is fail-closed: a procedure with no auth policy
// resolves to `auth: "user"`, so forgetting this file is a loud 401 once
// deployed rather than a silent public endpoint. The trap is that the alarm is
// inaudible locally -- `pnpm dev` has no gateway and therefore no gate, so an
// app missing this file is fully green in dev and 100% unreachable deployed.
// That is how `examples/kv-dashboard` and `examples/auth-uploads-kv` shipped
// (docs/pilot/e2e-scenarios.md, #163).
//
// This app has no users and no per-user data: every key it touches lives under
// the shared `sp/` prefix in one bucket. So anonymous is the correct posture
// here, and `publiclyAccessible: true` is the explicit confirmation the
// manifest validator requires. An app with real user data does the opposite --
// drop these entries and read identity inside the handler with
// `env.auth.getUser()`.
export default defineApp({
  resources: {
    "rpc:probe.ping": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.reset": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.text": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.binary": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.overwrite": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.deleteAbsent": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.contentTypes": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.listPrefix": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.listPaginate": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.listOvershoot": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.streamPut": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.streamGet": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.streamGetAbsent": { auth: "anon", publiclyAccessible: true },
    "rpc:probe.streamThenBuffered": { auth: "anon", publiclyAccessible: true },
  },
});
