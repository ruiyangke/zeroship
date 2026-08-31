# `@zeroship/control`

`@zeroship/control` is the TypeScript client for the control-plane HTTP API.
It is for platform-owned code: the Builder app, CLIs, internal agents, and
tests that need to create apps, deploy `.zship` artifacts, or manage
environment configuration.

Creator apps should not import this package. Creator-facing app code talks to
runtime SDKs such as `@zeroship/db`, `@zeroship/kv`, `@zeroship/auth`, and
`@zeroship/rpc`; the control plane is an operator/admin surface.

The Rust API lives in `crates/zeroship-control/src/{api,env_handlers}.rs`.
The TypeScript client lives in `sdks/control/src/index.ts`.

## Client setup

```ts
import { createControlClient } from "@zeroship/control";

const control = createControlClient({
  baseUrl: "http://localhost:9090",
  auth: () => process.env.CONTROL_KEY,
});

const apps = await control.apps.list();
```

`auth` is a master key or bearer token provider. Values without an auth scheme
are sent as `Authorization: Bearer <value>`.

For server-side proxies that authenticate through the dashboard session cookie,
forward the inbound cookie and mirror upstream `Set-Cookie` headers:

```ts
const control = createControlClient({
  baseUrl: CONTROL_URL(),
  cookie: () => getRequest()?.headers.get("cookie") ?? null,
  onSetCookie: (cookie) => {
    getResponseHeaders()?.append("Set-Cookie", cookie);
  },
});
```

## Namespaces

The public API is grouped by control-plane domain:

```ts
await control.apps.create({ name: "demo", plan_id: "free" });
await control.apps.deploy(appId, zshipBytes);
await control.apps.setPlan(appId, { plan_id: "pro" });
await control.apps.logs(appId);
await control.apps.archive(appId);
await control.apps.unarchive(appId);

await control.env.setVar(appId, { key: "PUBLIC_URL", value: "https://..." });
await control.env.setSecret(appId, { key: "OPENAI_API_KEY", value: "sk-..." });
await control.env.setExpose(appId, { keys: ["OPENAI_API_KEY"] });
await control.env.listAudit(appId, { limit: 100 });

await control.egressRules.set(appId, {
  verdict: "accept",
  destination: "db.example.com",
  port: 5432,
});
await control.egressRules.list(appId);
await control.egressRules.remove(appId, { destination: "db.example.com", port: 5432 });
```

### App archive lifecycle

Apps are archived, not hard-deleted. `control.apps.archive(appId)` sends
`PUT /api/apps/{id}/archive`; `control.apps.unarchive(appId)` sends `DELETE` to
the same resource. Both return the current `AppRecord`. Its `archived_at` is a
timestamp after archive and `null` after unarchive, and both operations are
safe to retry.

After the gateway route feed and workflow schedulers converge, archive stops
new gateway dispatch and scheduled workflow dispatch. Work already admitted
during that convergence window may finish and be metered. Route invalidation is
pull-based, so a gateway that cannot complete another control-plane poll keeps
its stale route snapshot; archive also does not terminate an already-open HTTP
stream, WebSocket, or other in-flight request. A deploy may still land while
the app is archived and replace its retained current code, but the new deploy
is not routed or scheduled until unarchive. The worker version feed deliberately
retains archived apps: removing one from that feed means database deprovisioning
and CDC teardown, which archive must not request. An idle isolate may therefore
remain cached, but it has no public gateway route after a successful route poll
and receives no new control-scheduled workflow work.

Archive does not erase the app row, name, deploy manifests, database schemas,
migration ledger, usage history, billing evidence, OAuth identity rows, or
relay aliases. OAuth and relay state is retained for restore and is not
independently disabled by this lifecycle marker. An archived app therefore
continues to hold its unique routable name. Unarchive restores the retained
route and workflow eligibility from the latest deploy, including one staged
while archived; it does not normally require another deploy. The old
hard-delete path could remove the per-app manifest keyspace before its database
cascade failed. Retrying archive after that already-observed partial failure is
safe, but a later cold or deploy-pinned load may need a staged redeploy to
restore the missing manifest keys before unarchive. There is no legacy repair
mode.

Database lifecycle is separate from app lifecycle. Archive does not drop a
schema or revoke the runtime database role. Privileged database teardown
belongs to `zeroship-migrate-server`, not control. Metering ingest remains
enabled so late and in-flight reports are not lost, and storage or other
retained resources may continue to accrue charges. Billing may still finalize
an open invoice from usage recorded before archive.

### `egressRules` is the raw-stream rule set

An app cannot open a raw socket or an outbound `WebSocket` to anywhere until it
holds an accept rule, and these three calls are how you write one. They carry
the same authority
as `env` (`env:read` to list, `env:write` to change) because both change what a
running app does without redeploying it.

A rule is three things: a **verdict**, a **destination** and a **port**.

`verdict` is `"accept"` or `"reject"` and is required - a body without one is
refused rather than assumed to mean accept.

`destination` is either an exact DNS name (`api.example.com`) or an address
range in CIDR form (`93.184.216.0/24`, `2606:4700::/32`). The server works out
which from the value itself.

The ranges above are deliberately real public space rather than the
documentation ranges you might expect (`198.51.100.0/24`, `2001:db8::/32`).
Those are refused by the platform SSRF floor, so a rule naming one is accepted
by this API and can never admit a connect - copy it and you get a rule that
silently does nothing.

Wildcards are not a grammar this API accepts:
`*.example.com` is refused, and the answer is either the exact hosts you meant
or the range they sit in. An accept range cannot be broader than `/16` on IPv4
or `/32` on IPv6; a reject range has no such floor, because `0.0.0.0/0` is the
strictest thing you can write and refusing it would make no sense.

**Rules are a set, not a list.** Order is not stored and has no effect. Any
matching reject refuses; otherwise any matching accept admits; otherwise the
connect is refused. So "everything in the vendor's `/20` except this `/24`" is
two rules and reads the same whichever way round you write them. What you cannot
express is a re-allow inside a reject - an accept range inside a reject range is
dead, and `list` reports it with `effective_verdict: "reject"` so you can see it.

Your plan caps how many **accept** rules an app may hold; `list` returns that
ceiling (`max_accept_rules`) and the count in use. Reject rules are not charged
against it, because a reject can only ever narrow what the app can reach.

Reject rules have a **separate** cap, also on `list` as `max_reject_rules` /
`used_reject_rules`. It is a resource bound and not a safety one: every rule
rides the projection the runtime polls, so an unbounded reject list is a load
problem, never a security one. Exceeding either cap is a `409`.

**One thing to know before your first range rule.** A name rule is decided
before the app looks anything up, so an app whose rules are all names never
resolves a destination it is going to refuse. A range rule can only be decided
against a resolved address, so once an app holds an accept range at a port, a
connect to that port for a name no rule allows is resolved first and refused
afterwards - and that lookup reaches the nameserver of whoever owns the name.
The API says this in the response to your first such rule; this paragraph is the
same statement made in advance.

**These rules cover every raw byte stream your app can open**: `node:net`,
`node:tls`, and outbound `WebSocket`. One rule set covers all of them - a rule
accepting `api.example.com:443` admits that destination over any of them, and
an app with no accept rule opens none of them. A refused WebSocket fires
`error` and closes with code `1006`, with the reason naming which check refused
(`ERR_NET_SSRF` for the platform floor, `ERR_NET_EGRESS_DENIED` for your own
rules).

`fetch` is the exception, and the only one. It reaches any public host with no
rule at all - these rules narrow raw streams, which is a blast-radius control on
your dependencies, not a boundary on your own code.

### Why `setExpose` follows `setSecret`

Those two lines are one operation, and skipping the second is the most
common way to end up with a credential the app cannot read.

A stored secret is visible on the `zeroship` `env` object — `env.OPENAI_API_KEY`
— as soon as it is set. It does **not** appear in `process.env` unless its
name is on the app's expose list. Vars are unconditional and appear on both.

| | `process.env` | `env` (from `"zeroship"`) |
| --- | --- | --- |
| var | always | always |
| secret | only if exposed | always |

The split is a blast-radius control. Every npm dependency in the bundle can
read `process.env` without the app author writing a line, so a secret reaches
it only on request. The `zeroship` `env` object is named explicitly by the
app's own code, which is a deliberate act.

This matters most for libraries that read the environment themselves. The AI
SDK's `loadAPIKey` looks up `process.env.OPENAI_API_KEY`, so a key that was
stored but never exposed reads as `undefined` inside the bundle even though
`env.OPENAI_API_KEY` is populated. That is the failure the two-line pattern
above prevents.

`setExpose` **replaces** the whole list rather than appending, so send the
full set of names each time. Reading the current list first and sending the
union is what the CLI does:

```bash
zeroship secret set OPENAI_API_KEY=sk-... --app=<uuid> --expose
zeroship secret expose-list --app=<uuid>
```

Prefer a secret over a var for anything credential-shaped: secrets are
encrypted at rest and are never readable back through the API (`list` returns
names only), while var values are stored as-is and returned by `listVars`.

There is no auth namespace: control is a pure API resource server (R5
cutover) — login/identity lives in `@zeroship/auth` against the auth
service, never against control. See `docs/reference/auth.md`.

`control.request<T>(path, options)` is the escape hatch for endpoints that do
not yet deserve a typed wrapper. Prefer adding a typed method once a caller
appears in product code.

## Deploys

`control.apps.deploy(appId, artifact)` sends `application/x-zship` by default.
The control plane no longer accepts raw JavaScript deploy bodies; callers should
upload the `.zship` artifact emitted by the build pipeline.

```ts
const artifact = await fs.promises.readFile("dist/app.zship");
const result = await control.apps.deploy(appId, artifact);
console.log(result.deploy_hash);
```

## Errors

Non-2xx responses throw `ControlError`:

```ts
import { ControlError } from "@zeroship/control";

try {
  await control.apps.get(appId);
} catch (error) {
  if (error instanceof ControlError && error.status === 401) {
    return null;
  }
  throw error;
}
```

`ControlError` carries `status`, `statusText`, parsed `body`, optional `code`,
optional `trace_id`, and the original `response`. `trace_id` is present only on
responses produced by `infrastructure_error_response`. For example,
`control.env.listVars` failures remain id-less. When present, quote `trace_id`
to an operator: the producing helper logged the real cause under the same key.
It is `undefined` when the server did not send one and is never invented by the
SDK. Branch on status/code, not message text.

## Design rules

- Keep this package framework-neutral. Do not import React, Vite, or Builder
  internals.
- Keep auth explicit. Browser/server cookie forwarding belongs in the caller's
  setup, not hidden global state.
- Use typed namespace methods for stable control-plane endpoints.
- Use `control.request()` only as a temporary escape hatch.
- Add tests when adding a method, especially for headers, body encoding,
  `204` handling, and error parsing.
