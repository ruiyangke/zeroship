import { defineApp } from "@zeroship/server";

// App resource policy. Without this file every procedure below resolves to
// `auth: "user"` when deployed, and the gateway refuses all sixteen with 401 --
// which is what happened: this example ran green under `pnpm dev` and was
// completely unreachable once deployed. See docs/pilot/e2e-scenarios.md.
//
// The fail-closed default is deliberate (a procedure with no auth policy needs
// an authenticated end-user, so forgetting auth is a loud 401 rather than a
// silent public endpoint). This demo has no login and no per-user data -- every
// key it touches lives under the shared `kv-demo:` prefix -- so it opts in to
// anonymous access the same way `examples/starter` does, with
// `publiclyAccessible: true` as the explicit confirmation the manifest
// validator requires.
//
// An app with real user data does the opposite: drop these entries and read
// identity inside the handler with `env.auth.getUser()`. `examples/auth-uploads-kv`
// is that shape.
export default defineApp({
  resources: {
    "rpc:kv.snapshot": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.visit": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.flag.set": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.rate.hit": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.cache.quote": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.memo.get": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.lease.acquire": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.lease.clear": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.session.create": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.session.delete": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.string.set": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.string.expire": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.string.persist": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.string.delete": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.keys.list": { auth: "anonymous", publiclyAccessible: true },
    "rpc:kv.clear": { auth: "anonymous", publiclyAccessible: true },
  },
});
