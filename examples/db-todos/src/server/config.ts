import { defineApp } from "@zeroship/server";

// App resource policy. Without this file every procedure in `src/index.ts`
// resolves to `auth: "user"`, and the gateway refuses all twenty-four with
// `{"code":"UNAUTHENTICATED","message":"authentication required"}` -- which is
// exactly how this example shipped: green under `pnpm dev` (18 of 19 smoke
// checks) and 100% unreachable once deployed. Measured by
// `tests/e2e_dev_vs_deployed_db.sh` on its first run, 2026-08-10.
//
// THE BUILD ALREADY SAID SO, in detail, and the example shipped anyway. Deleting
// this file and rebuilding printed this when there were fourteen procedures
// (verified by running it, 2026-08-10; the count is now twenty-four, and the count
// in the message is the only part of it that has changed):
//
//   [zeroship:manifest] 14 procedures declare no `auth` policy and will deploy
//   fail-closed:
//     - "rpc:todos.archive" (export archiveTodo in .../src/index.ts)
//     ... all fourteen, each with its export name and file path ...
//
// So this is NOT a visibility gap, and an earlier version of this comment was
// wrong to imply one. `warnFailClosedProcedures` in
// `sdks/vite-plugin/src/manifest.ts:844` names every affected procedure and
// explains the mechanism; it is a WARNING rather than a build failure by
// deliberate choice, because making it fatal "would break every example that
// currently relies on the default" (same file, :863). What actually happened is
// that a loud, accurate, non-fatal build warning was scrolled past -- which is
// the known cost of that choice, not a defect in it.
//
// The fail-closed default is likewise deliberate: a procedure with no auth
// policy needs an authenticated end-user, so forgetting auth is a loud 401
// rather than a silent public endpoint. The same omission shipped in
// `kv-dashboard` and `auth-uploads-kv` (#163, docs/pilot/e2e-scenarios.md).
//
// This demo has no login: `users.seed` and the shared "ledger" user in
// `users.public` exist precisely because there is no per-user identity to read.
// So anonymous is the correct posture, and `publiclyAccessible: true` is the
// explicit confirmation the manifest validator requires.
//
// An app with real user data does the opposite -- drop these entries and read
// identity inside the handler with `env.auth.getUser()`. `examples/
// auth-uploads-kv` is that shape.
export default defineApp({
  resources: {
    "rpc:todos.list": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.listPage": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.get": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.count": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.listWithUser": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.subscribe": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.create": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.setDone": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.archive": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.delete": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.shareToWebhook": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txCommit": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txRollback": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txNested": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txIsolation": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txDepth": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.countTitle": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txParallel": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txOverlap": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txPlainWrite": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txBranchWrites": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txOrphanedWrite": { auth: "anonymous", publiclyAccessible: true },
    "rpc:todos.txRaceStep": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.seed": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.getPair": { auth: "anonymous", publiclyAccessible: true },
    "rpc:users.public": { auth: "anonymous", publiclyAccessible: true },
  },
});
