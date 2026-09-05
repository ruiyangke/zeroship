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
    "rpc:probe.ping": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.reset": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.text": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.binary": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.overwrite": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.deleteAbsent": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.contentTypes": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.listPrefix": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.listPaginate": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.listOvershoot": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.streamPut": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.streamGet": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.streamGetAbsent": { auth: "anonymous", publiclyAccessible: true },
    "rpc:probe.streamThenBuffered": { auth: "anonymous", publiclyAccessible: true },
  },
});
