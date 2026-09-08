# `@zeroship/control`

`@zeroship/control` is the TypeScript client for the control-plane HTTP API.
It is for platform-owned code: the Builder app, CLIs, internal agents, and
tests that need to create apps, deploy `.zship` artifacts, or manage
environment configuration.

Creator apps should not import this package. Creator-facing app code talks to
runtime SDKs such as `@zeroship/db`, `@zeroship/kv`, `@zeroship/auth`, and
`@zeroship/rpc`; the control plane is an operator/admin surface.

The Rust API lives in `crates/zeroship-control/src/{api,env_handlers}.rs`, and
the organization surface in `crates/zeroship-control/src/organizations.rs`.
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
await control.organizations.create({ name: "Acme" });
await control.organizations.addMember(organizationId, {
  user_id: userId,
  role: "developer",
});
await control.organizations.createProject(organizationId, { name: "Checkout" });
await control.projects.addMember(projectId, { user_id: userId, role: "viewer" });

await control.apps.create({ name: "demo", plan_id: "free" });
await control.apps.deploy(appId, zshipBytes);
await control.apps.setPlan(appId, { plan_id: "pro" });
await control.apps.logs(appId);
await control.apps.archive(appId);
await control.apps.unarchive(appId);
await control.apps.delete(appId); // terminal; the app must be archived first

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

### `organizations` and `projects`: who owns what, and who may change it

An **organization** owns projects, a **project** owns apps, and the organization
is the party that is billed. An app reaches its organization only through its
project: there is no `organization_id` on an app and no per-app membership, so
the path is `apps.project_id -> projects.organization_id` and there is exactly
one of it.

A creator who has never thought about any of this still has all three. The first
deploy on a fresh account mints a **personal organization** and a default
project behind the scenes. A personal organization is an ordinary row with a
`personal_owner_id` set; nothing reads differently because of it, and
transferring ownership clears the pointer, which *is* the conversion from
personal to shared.

Ids are typed and are what these methods take. `org_...`, `prj_...`, `ivt_...`,
and a member's `user_id`, which is a UUID. A **slug is not an id**: the server
parses the typed id before it authorizes anything, so a slug in a path is a
`400`, and a well-formed id for an organization you have no seat in is a `403`
with nothing in it to tell the two apart. That is deliberate.

#### The two integers

Every seat carries a role, and every role carries two ranks:

| role | rank | billing_rank |
| --- | --- | --- |
| viewer | 10 | 0 |
| developer | 20 | 0 |
| billing | 10 | 20 |
| admin | 30 | 10 |
| owner | 40 | 20 |

`rank` orders authority over apps and the organization; `billing_rank` orders
authority over money. They are independent on purpose: a bookkeeper reads apps
and cannot deploy, and an admin reads the invoice and cannot change the payout
account. One number could express neither seat.

**An actor may act on a target only when `actor.rank > target.rank` AND
`actor.billing_rank >= target.billing_rank`.** The comparison runs inside the
statement that performs the change, not in a check before it, so a demotion that
commits while a request is in flight refuses that request rather than racing it.
A refusal is a `403` whose `detail` names which of the two comparisons failed.

The ranks are reported on every `OrganizationMemberRecord` and are **not** to be
hard-coded in a client. They are rows in the platform's role ladder; a client
that copied the table above would disagree with the server the day a migration
moves one.

Two consequences a UI will meet:

- **The inequality is strict, so an organization has one owner.** Nothing
  outranks rank 40, so `addMember` and `changeMemberRole` both refuse the owner
  role and `transferOwnership` MOVES the seat rather than duplicating it. An
  owner list will always show one row.
- **Leaving is its own call, because the inequality would otherwise forbid it.**
  An actor never outranks themselves, so `removeMember(org, myOwnId)` is
  refused. `organizations.leave(org)` is the carve-out: it takes no user id,
  and the route it sends to (`DELETE /api/organizations/{id}/membership`) has no
  segment that could name one, so the seat it reaches is the caller's by
  construction. It needs no rank — a viewer can leave — and it carries its own
  scope, `organization:members:leave`, so a read-only consent cannot delete its
  holder's seat. The general inequality is untouched and still stops an admin
  removing a peer.

  A **sole owner is refused** (`409 last owner`) and told to transfer ownership
  first. An organization that keeps no owner is one no route can repair.

#### Closing an organization

`organizations.dissolve(org)` closes one. Owner only, and it cannot be undone.

It is a **soft close**: the row survives with `dissolved_at` set, and so do its
members, its invitations and its billing history — the organization is the
billing subject, and a row that vanished would take the counterparty out of a
money record that has to outlive the relationship. Every read still returns it,
including `list()`, so a UI should label a closed organization rather than hide
it. Every write against it answers `409 organization dissolved`, including a
second close and including a departure.

It is **refused while the organization still owns projects** (`409 organization
has projects`, carrying the count). Delete them with `projects.delete`, which
itself needs each project to own no apps. The ordering is the point: an
organization that closed while apps were still running would leave them with
nobody who answers for them.

A close **releases the names the organization was holding**. Both the slug and
the personal-organization slot are unique among *live* organizations only, so a
creator who closes "Acme" can create another "Acme", and a creator who closes
their personal organization gets a fresh one on their next deploy instead of an
account that can never deploy again. Neither column is rewritten: the closed
record still says what it was called and whose it was.

#### Projects narrow; they never widen

A member at **admin rank or above holds authority over every project** in the
organization, with no `project_members` row anywhere. A member **below admin**
holds authority only on projects they have a row for, and their authority there
is `min(organization rank, project rank)`.

So a project seat is a grant and a ceiling. Seating an organization `developer`
(rank 20) as a project `owner` gives them rank 20 on that project, not 40.

`billing_rank` is organization-level and is never narrowed: there is no
per-project invoice, and `project_members` carries no billing dimension.

`organizations.projects(id)` returns the projects the caller can *reach*, not
every project of the organization. That filter is the point: listing them all
would hand a below-admin member the names of projects they cannot open.

`projects.changeMemberRole` moves an existing seat in **one** call. It is not
delete-then-add, and the difference is visible to a UI: a member below admin
with no project row reaches nothing, so removing before granting would blank
their access in between, and a failure between the two would leave the seat gone
rather than narrowed.

`projects.update` renames or re-slugs; `projects.delete` removes the project and
its seats, and is **refused while the project owns apps** (`409 project has
apps`, carrying the count). Both need admin rank or above, which is the same
threshold as creating one and for the same reason: below admin, the project row
is what grants reach, so reshaping it would be reshaping your own authority.

#### Invitations, and the one time the token exists

The platform **mails the invitation**, and the response says what became of the
attempt. `delivery` is `sent`, `suppressed` (the address is on the platform's
bounce/complaint list, so nothing was sent) or `failed` (the transport refused).
The invitation row is written and committed *before* the send is attempted, so a
delivery failure costs an email rather than an invitation — a `failed` invite is
still redeemable with the token in the same response.

`createInvite` is also the only response that carries a `token`. The server
stores a digest of it and nothing else, so no later read can return it — an
`InviteRecord` has no field to hold one. A client does not have to deliver it
when `delivery` is `sent`, but it **must surface it otherwise**, because in
those cases nothing else will; the remedy for a lost one is `revokeInvite`
followed by a new `createInvite`. Holding the token grants nothing on its own:
redemption additionally requires the redeeming account's *verified* address to
be the invited address.

`redeemInvite` is not organization-scoped, because the redeemer holds no seat
yet: the token is the capability and the organization comes back in the
response. Redemption re-derives the **inviter's live authority** at that moment,
so an invitation from an admin who has since been demoted is refused even though
it was valid when issued. Every unredeemable case — expired, already used, wrong
address, lapsed inviter — is the same `403` with the same body, so the endpoint
cannot be used to learn about other people's invitations.

#### Authority is resolved per request, and is never cached

The server re-reads the caller's seat on every request. Do not cache a rank, a
role or a decision in a client and act on it: the entity cache that used to sit
in front of this was deleted so that revoking a seat takes effect immediately
with no invalidation signal to miss. Ask again.

### App archive lifecycle

Archive is reversible, delete is terminal, and the order is enforced rather
than advised. `control.apps.archive(appId)` sends `PUT /api/apps/{id}/archive`;
`control.apps.unarchive(appId)` sends `DELETE` to the same resource. Both return
the current `AppRecord`. Its `archived_at` is a timestamp after archive and
`null` after unarchive, and both operations are safe to retry.

`control.apps.delete(appId)` sends `DELETE /api/apps/{id}` and ends the app.
Read the two `DELETE`s carefully: on the app's `archive` RESOURCE it restores;
on the APP it ends. Delete refuses an app that is not archived and names
archive as the missing step, needs `admin` in the owning organization, and
returns no body. It is not retryable in the sense the two above are: a second
call is refused, because ending the app cuts the edge every authority check on
it is resolved through. See "App deletion" below for what survives it and why.

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

### App deletion

Delete is the last step of the account-closure funnel. A creator closing their
account is refused while they are the sole owner of a live organization; the
organization is refused while it owns projects; a project is refused while it
owns apps. `DELETE /api/apps/{id}` is what ends that chain, and it is the reason
every refusal above now names a step that can actually be taken.

**The app row survives its own deletion, and that is the design rather than an
omission.** Two of its children point in opposite directions: a finalized
`invoice_lines` row pins the app with `ON DELETE RESTRICT`, so a row delete is
refused outright once the app has been invoiced, while `usage_aggregates`
cascades, so a row delete instead destroys the input the unbilled-usage
predicate reads. A hard delete is therefore impossible or destructive depending
only on whether the reconciler has run. Deletion is a marker: nothing cascades,
and `zeroship_control` holds no `DELETE` privilege on the table at all.

What ends is reachability. The app leaves its project, which is what lets the
project be deleted afterwards; its current artifact pointer is cleared, so no
route or worker can serve it again; and its whole environment - vars, secrets,
and the `process.env` expose list - is destroyed, because that is live
capability rather than a record of anything.

What is kept is evidence and identity. Every billing record outlives the app:
usage aggregates, usage history, invoice lines, plan-change events, spend-state
history. The reconciler and the "does this organization owe" predicate both
reach an app through its organization rather than through its project, so a
deleted app is still billed and still owes; an unpaid invoice cannot be walked
away from by deleting the app that incurred it. The audit trail records who
ended the app, when, and which project it left. And the app's NAME - its
routable hostname - is retired rather than released: old links, cookies and
OAuth redirect URIs still point at it, so it is never handed to a later
registrant.

Deploy blobs are content-addressed and shared by hash across apps and deploys,
so reclaiming them is a sweep over the store, not part of this call. The app's
database schema and role are privileged teardown and belong to
`zeroship-migrate-server`, exactly as they do for archive.

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
