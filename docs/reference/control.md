# `@zeroship/control`

`@zeroship/control` is the TypeScript client for the control-plane HTTP API. It
is the operator/admin surface of the platform: the calls that create apps,
deploy `.zship` artifacts, manage environment configuration, and administer
organizations, projects, members and invitations.

It is not part of an app's runtime. App code talks to `@zeroship/db`,
`@zeroship/kv`, `@zeroship/auth` and `@zeroship/rpc`; the control plane is what
provisions and operates the things those runtimes use.

## Client setup

```ts
import { createControlClient } from "@zeroship/control";

const control = createControlClient({
  baseUrl: "http://localhost:9090",
  auth: () => process.env.CONTROL_KEY,
});

const apps = await control.apps.list();
```

`baseUrl` is required and names the control-plane origin. `auth` is a master key
or bearer-token provider: a value that already carries an auth scheme is sent
unchanged, and a bare value becomes `Authorization: Bearer <value>`.

For a server-side proxy that authenticates through a dashboard session cookie,
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

Two further options exist for callers that need them, and most do not: `fetch`
overrides the default `globalThis.fetch`, and `headers` supplies headers applied
to every request. Providers may be values or functions, and a function may be
async. Creating a client without a `baseUrl` or a fetch implementation throws
immediately.

## Namespaces

The public API is grouped by control-plane domain. The namespaces are `apps`,
`env`, `egressRules`, `organizations` and `projects`. There is no `auth`
namespace - control is a pure API resource server, and login and identity live
in `@zeroship/auth` against the auth service, never against control (see
`docs/reference/auth.md`). There is also no `workflows` namespace.

The compact tour below runs from the call almost every caller makes to the ones
a full administration flow reaches:

```ts
import { createDeployCommand } from "@zeroship/control";

// Apps: create, deploy, observe, retire.
await control.apps.create({ name: "demo", plan_id: planId });
await control.apps.get(appId);
await control.apps.list();
await control.apps.setPlan(appId, { plan_id: planId });
await control.apps.logs(appId);
await control.apps.usage(appId);
const command = await createDeployCommand(zshipBytes);
await control.apps.deploy(appId, command);
await control.apps.archive(appId);
await control.apps.unarchive(appId);
await control.apps.delete(appId); // terminal; the app must be archived first

// Environment and secrets.
await control.env.setVar(appId, { key: "PUBLIC_URL", value: "https://..." });
await control.env.listVars(appId);
await control.env.setSecret(appId, { key: "OPENAI_API_KEY", value: "sk-..." });
await control.env.setExpose(appId, { keys: ["OPENAI_API_KEY"] });
await control.env.listAudit(appId, { limit: 100 });

// Egress rules.
await control.egressRules.set(appId, {
  verdict: "accept",
  destination: "db.example.com",
  port: 5432,
});
await control.egressRules.list(appId);
await control.egressRules.remove(appId, { destination: "db.example.com", port: 5432 });

// Organizations, members and projects.
await control.organizations.create({ name: "Acme" });
await control.organizations.list();
await control.organizations.addMember(organizationId, {
  user_id: userId,
  role: "developer",
});
await control.organizations.createProject(organizationId, { name: "Checkout" });
await control.projects.addMember(projectId, { user_id: userId, role: "viewer" });
```

The full method surface is:

- `apps`: `list`, `get`, `create`, `deploy`, `setPlan`, `usage`, `logs`,
  `archive`, `unarchive`, `delete`.
- `env`: `listVars`, `setVar`, `deleteVar`, `listSecrets`, `setSecret`,
  `deleteSecret`, `listExpose`, `setExpose`, `listAudit`.
- `egressRules`: `list`, `set`, `remove`.
- `organizations`: `list`, `create`, `get`, `update`, `members`, `addMember`,
  `changeMemberRole`, `removeMember`, `leave`, `dissolve`, `transferOwnership`,
  `invites`, `createInvite`, `revokeInvite`, `redeemInvite`, `projects`,
  `createProject`.
- `projects`: `get`, `update`, `delete`, `members`, `addMember`,
  `changeMemberRole`, `removeMember`.

`env.listAudit` takes an optional `limit` that defaults to 50 and is clamped to
`1..=500`; it returns the app's audit entries newest-first.

`control.request<T>(path, options)` is the escape hatch for endpoints that do
not yet deserve a typed wrapper. Prefer adding a typed method once a caller
appears in product code.

### `organizations` and `projects`: who owns what, and who may change it

An **organization** owns projects, a **project** owns apps, and the organization
is the party that is billed. An app reaches its organization through its
project, and only through it: an app record carries no organization id and there
is no per-app membership. `apps.create` takes no project parameter, so an app
created through this client lands in the caller's personal project.

A creator who has never thought about any of this still has all three. The first
deploy on a fresh account mints a **personal organization** and a default
project behind the scenes. A personal organization is an ordinary organization
whose `personal_owner_id` is set; nothing reads differently because of it, and
transferring ownership clears the pointer, which *is* the conversion from
personal to shared.

Ids are typed and are what these methods take. Organizations use `org_...`,
projects use `prj_...`, invitations use `ivt_...`, and users use the exported
user id type: `usr_` followed by the fixed-width lowercase base36 UUIDv7 body.
A **slug is not an id**: the server parses the typed id before it authorizes
anything, so a slug in a path is a `400`, and a well-formed id for an
organization you have no seat in is a `403` with nothing in it to tell the two
apart. That is deliberate.

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
`actor.billing_rank >= target.billing_rank`.** The comparison is part of the
change itself, not a check before it, so a demotion that commits while a request
is in flight refuses that request rather than racing it. A refusal is a
`403 insufficient authority` whose `detail` explains the reason in prose, for
example that granting a role needs a strictly higher rank and at least equal
billing authority.

The ranks are reported on every `OrganizationMemberRecord` and are **not** to be
hard-coded in a client. They are values the server owns; a client that copied
the table above would disagree with the server the day a migration moves one.

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
  construction. It needs no rank - a viewer can leave - and it carries its own
  scope, `organization:members:leave`, so a read-only consent cannot delete its
  holder's seat. The general inequality is untouched and still stops an admin
  removing a peer.

  A **sole owner is refused** (`409 last owner`) and told to transfer ownership
  first. An organization that keeps no owner is one no route can repair.

#### Closing an organization

`organizations.dissolve(org)` closes one. Owner only, and it cannot be undone.

It is a **soft close**: the record survives with `dissolved_at` set, and so do
its members, its invitations and its billing history - the organization is the
billing subject, and a record that vanished would take the counterparty out of a
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
account that can never deploy again. Neither name is rewritten: the closed
record still says what it was called and whose it was.

#### Projects narrow; they never widen

A member at **admin rank or above holds authority over every project** in the
organization, with no per-project seat anywhere. A member **below admin** holds
authority only on projects they hold a seat for, and their authority there is
`min(organization rank, project rank)`.

So a project seat is a grant and a ceiling. Seating an organization `developer`
(rank 20) as a project `owner` gives them rank 20 on that project, not 40.

`billing_rank` is organization-level and is never narrowed: there is no
per-project invoice, and a project seat carries no billing dimension.

`organizations.projects(id)` returns the projects the caller can *reach*, not
every project of the organization. That filter is the point: listing them all
would hand a below-admin member the names of projects they cannot open.

`projects.changeMemberRole` moves an existing seat in **one** call. It is not
delete-then-add, and the difference is visible to a UI: a member below admin
with no project seat reaches nothing, so removing before granting would blank
their access in between, and a failure between the two would leave the seat gone
rather than narrowed.

`projects.update` renames or re-slugs; `projects.delete` removes the project and
its seats, and is **refused while the project owns apps** (`409 project has
apps`, carrying the count). Both need admin rank or above, which is the same
threshold as creating one and for the same reason: below admin, the project seat
is what grants reach, so reshaping it would be reshaping your own authority.

#### Invitations, and the one time the token exists

The platform **mails the invitation**, and the response says what became of the
attempt. `delivery` is `sent`, `suppressed` (the address is on the platform's
bounce/complaint list, so nothing was sent) or `failed` (the transport refused).
The invitation is recorded *before* the send is attempted, so a delivery failure
costs an email rather than an invitation - a `failed` invite is still redeemable
with the token in the same response.

`createInvite` is also the only response that carries a `token`. The server
stores a digest of it and nothing else, so no later read can return it - an
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
it was valid when issued. Every unredeemable case - expired, already used, wrong
address, lapsed inviter - is the same `403` with the same body, so the endpoint
cannot be used to learn about other people's invitations.

#### Authority is resolved per request, and is never cached

The server re-reads the caller's seat on every request. Do not cache a rank, a
role or a decision in a client and act on it: revoking a seat takes effect
immediately, and there is no invalidation signal to wait for. Ask again.

### An app's execution zone is chosen once

`control.apps.create` takes `name`, an optional `plan_id` naming a plan in the
platform's plan catalog, and an optional `execution_zone`. A name is 1-64
characters of ASCII letters, digits, hyphen or underscore, must not be one the
platform edge already routes, and must not begin with `app_` (reserved for app
ids).

`execution_zone` is the NAME of an operator-declared execution zone: a set of
worker deployment units that share creator-side connectivity. It is written
when the app is created and frozen there, because moving an app between zones is
a data migration of its creator storage rather than a metadata edit, and because
an app can only run in the zone it was created in.

Omit it in a deployment that declares one zone and the app lands in it. A
deployment that declares several refuses a create that does not name one,
rather than choosing: an app in the wrong zone is one that no worker of its
creator's fleet will ever run.

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

After archive, the app stops receiving new public traffic and new scheduled work
once the platform's routing and scheduling state has caught up. Work already
admitted while that state converges may finish and be metered. Archive does not
terminate an already-open HTTP stream, WebSocket, or other in-flight request. A
deploy may still land while the app is archived and replace its retained current
code, but the new deploy is not routed or scheduled until unarchive.

Archive does not erase the app record, name, deploy manifests, database schemas,
migration ledger, usage history, billing evidence, OAuth identity records, or
relay aliases. OAuth and relay state is retained for restore and is not
independently disabled by this lifecycle marker. An archived app therefore
continues to hold its unique routable name. Unarchive restores the retained
route and workflow eligibility from the latest deploy, including one staged
while archived; it does not normally require another deploy. A deploy staged
while archived is activated by the restore.

Database lifecycle is separate from app lifecycle. Archive does not drop a
schema or revoke the runtime database role; privileged database teardown is a
separate operation. Metering remains active so late and in-flight reports are
not lost, and storage or other retained resources may continue to accrue
charges. Billing may still finalize an open invoice from usage recorded before
archive.

### App deletion

Delete is the last step of the account-closure funnel. A creator closing their
account is refused while they are the sole owner of a live organization; the
organization is refused while it owns projects; a project is refused while it
owns apps. `DELETE /api/apps/{id}` is what ends that chain, and it is the reason
every refusal above names a step that can actually be taken.

**The app record survives its own deletion, and that is the design rather than
an omission.** Billing records must outlive the app they describe, and an
unpaid invoice cannot be walked away from by deleting the app that incurred it.
Deletion is therefore a marker, not an erase.

What ends is reachability. The app leaves its project, which is what lets the
project be deleted afterwards; its current artifact pointer is cleared, so no
route or worker can serve it again; and its whole environment - vars, secrets,
and the `process.env` expose list - is destroyed, because that is live
capability rather than a record of anything.

What is kept is evidence and identity. Every billing record outlives the app:
usage aggregates, usage history, invoice lines, plan-change events, and
spend-state history. The platform still bills a deleted app and it still owes.
The audit trail records who ended the app, when, and which project it left. And
the app's NAME - its routable hostname - is retired rather than released: old
links, cookies and OAuth redirect URIs still point at it, so it is never handed
to a later registrant.

Deploy blobs are content-addressed and shared by hash across apps and deploys,
so reclaiming them is a sweep over the store rather than part of this call. An
app's database schema and role are privileged teardown and are separate from it.

### `egressRules` is the raw-stream rule set

An app cannot open a raw socket or an outbound `WebSocket` to anywhere until it
holds an accept rule, and these three calls are how you write one. They carry
the same authority as `env` (`env:read` to list, `env:write` to change) because
both change what a running app does without redeploying it.

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
`used_reject_rules`. It is a resource bound and not a safety one. Exceeding
either cap is a `409`.

`list` also reports the plan's socket and byte ceilings as `max_sockets` and
`egress_ceiling_bytes`.

`set` returns the rule it stored plus an optional `notice`. `notice` is non-null
only on an app's FIRST accept rule for an address range, and it is the one place
that change in behavior is explained. Show it.

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
encrypted at rest and are never readable back through the API (`listSecrets`
returns names only), while var values are stored as-is and returned by
`listVars`.

## Deploys

A deploy is a command. `createDeployCommand(archive, id?)` snapshots the
`.zship` bytes and binds them to a deploy command id (`dcm_...`), and
`control.apps.deploy(appId, command)` sends that snapshot as
`application/x-zship` with the command id in the `Idempotency-Key` header. The
archive is the artifact the build pipeline emits; there is no stream, form or
raw JavaScript deploy body.

```ts
import { createDeployCommand, DeployOutcomeUnknownError } from "@zeroship/control";

const command = await createDeployCommand(await fs.promises.readFile("dist/app.zship"));
let result;
try {
  result = await control.apps.deploy(appId, command);
} catch (error) {
  if (!(error instanceof DeployOutcomeUnknownError)) throw error;
  // The command may already be accepted; sending it again is safe.
  result = await control.apps.deploy(appId, command);
}
console.log(result.deploy_id, result.deploy_hash, result.replayed);
```

A successful deploy answers `200` with the command's acceptance:

```json
{
  "command_id": "dcm_...",
  "deploy_id": "dep_...",
  "deploy_hash": "sha256:...",
  "blobs_uploaded": 3,
  "blobs_deduped": 12,
  "lifecycle_revision": 4
}
```

**The command id names one deploy, never its artifact.** Mint a new id for
every deploy, including a rollback that uploads an earlier artifact again, and
reuse an id only to resend the same deploy. Control refuses a request without
exactly one canonical `Idempotency-Key` with `400 invalid_idempotency_key`
before it reads the body.

**Control records an immutable receipt.** It binds the app, the caller, the
operation, the normalized content type and the hash of the bytes it actually
consumed to the command id, together with the acceptance it returned. A refused
or failed deploy leaves none of that behind.

**An exact repeat replays.** The same id with the same bytes, from the same
caller, for the same app returns the original acceptance with the
`idempotent-replayed: true` header (`replayed` in the SDK) and changes
nothing: no new revision and no retargeting. The same
id with different bytes, another app or another caller is refused with `409
idempotency_key_conflict`, which says nothing about the original. A command
for a deleted app is `404` even when its receipt exists.

**Some failures leave the outcome unknown.** A network failure or a `5xx` does
not say whether Control committed the command. The SDK throws
`DeployOutcomeUnknownError`, which carries the `commandId`, and the same
command can be sent again. Every other non-2xx is a refusal (`ControlError`)
and nothing was accepted. `zeroship deploy` prints its `command_id` before it
uploads, resends an unanswered command a bounded number of times, and resumes
one with `--command-id=<id>`.

**Acceptance is durable publication, not activation.** A live app's deploy
takes the app's next lifecycle revision and publishes an activation. A `200`
means the deployment is durably published, not that its schedules already run.
An archived app's deploy is staged: its `lifecycle_revision` is `null`, and
restoring the app activates the staged deployment under a fresh revision.

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

`ControlError` carries `status`, `statusText`, the parsed `body`, an optional
`trace_id`, and the original `response`. The `body` is the refusal itself: an
`error` slug such as `forbidden`, `insufficient authority`, or
`idempotency_key_conflict`, with an optional `detail` sentence that says more in
prose. Branch on `status` together with `body.error`, never on message text.

`trace_id` is present only on an infrastructure failure (`{"error": "internal error"}`),
never on an ordinary refusal. For example, `control.env.listVars` failures
remain id-less. When present, quote `trace_id` to an operator: it is the
server's correlation id for the failure. It is `undefined` when the server did
not send one and is never invented by the SDK.

## Design rules

- Use typed namespace methods for stable control-plane endpoints; treat
  `control.request()` as a temporary escape hatch.
- Keep auth explicit. The client never stores credentials for you, and cookie
  forwarding for a server-side proxy belongs in the caller's setup.
- The client is framework-neutral and has no UI-framework or console
  dependency.