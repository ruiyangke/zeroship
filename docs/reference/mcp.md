# `@zeroship/mcp`

`@zeroship/mcp` is a stdio MCP server that exposes the zeroship control plane as
agent-native tools. MCP is the Model Context Protocol, and "stdio" means the
server speaks it over a child process's standard input and output. An MCP client
— Claude Code, an editor, any host that speaks the protocol — launches the
server and calls its tools; the server turns each call into a control-plane
request and returns the result as JSON text.

The **control plane** is the HTTP service that owns apps, deploys, plans and
billing, reached at `ZEROSHIP_CONTROL_URL`. The server wraps the
[`@zeroship/control`](control.md) client. This page is self-contained for the
MCP surface: it names every tool, argument and result field, and the exact text
a failed call returns. Where a rule is the control plane's to enforce — deploy
idempotency, archive semantics, authorization — this page states the result you
observe and links to [`@zeroship/control`](control.md) for the mechanism.

## What you need first

The server reads two environment variables from the process that launches it:

| Name | Meaning | Default |
| --- | --- | --- |
| `ZEROSHIP_CONTROL_URL` | Control-plane origin. | `http://localhost:9090` |
| `ZEROSHIP_TOKEN` | Bearer personal access token (PAT). | none — required |

The server treats `ZEROSHIP_TOKEN` as an opaque bearer token: it sends the value
unchanged as `Authorization: Bearer <value>` on every control-plane call and
never inspects, refreshes or stores it. Create one with `zeroship login` and put
it in the client's environment, or supply an existing PAT. What a token may do —
which apps it can see, which operations it may perform, when it expires and how
it is revoked — is the control plane's authorization, not the MCP server's, and
the MCP server adds no filter of its own. Without the variable, every tool
answers with the exact error `set ZEROSHIP_TOKEN (run \`zeroship login\`)`.

## Wiring it into an MCP client

Claude Code registers it in one command:

```bash
claude mcp add zeroship -- zeroship-mcp
```

The process environment must carry `ZEROSHIP_TOKEN`, and
`ZEROSHIP_CONTROL_URL` when the control plane is not on the local default.

A client that reads a project config takes the same two fields:

```json
{
  "mcpServers": {
    "zeroship": {
      "command": "zeroship-mcp",
      "env": {
        "ZEROSHIP_CONTROL_URL": "http://localhost:9090",
        "ZEROSHIP_TOKEN": "<PAT from zeroship login>"
      }
    }
  }
}
```

The shape is the same for any stdio host: a command to launch and the
environment to launch it with. The server speaks the protocol on stdout and
listens on no port, so the client owns its lifecycle.

`zeroship-mcp --list-tools` prints the tool names the installed version
registers, without starting the server. Use it to confirm what an install
exposes.

## Choosing an app: the target shape

Every tool that acts on an existing app takes one `target` argument, in exactly
one of two forms:

```json
{ "target": { "kind": "id", "appId": "app_0000000002e4nenowz3qmamtd" } }
```

```json
{ "target": { "kind": "name", "appName": "my-app" } }
```

- **`kind: "id"`** takes `appId`, a canonical app id: `app_` followed by exactly
  25 lowercase base36 characters, for example
  `app_0000000002e4nenowz3qmamtd`. A raw UUID or any other string is refused
  before any control-plane request is sent, with the reason
  `appId must be a canonical AppId`.
- **`kind: "name"`** takes `appName`, a non-empty string. The server resolves it
  by exact match among the app records the control plane returns for the token.
  It lowercases or trims nothing, so resolution is case-sensitive even though
  app names are hostname labels.

There is no third form: a bare id or a bare name is refused. An unknown name is
an error for every tool except `deploy_app`, which creates that name instead.

## Tools

**How names are spelled.** Multi-word argument names are camelCase — `zshipPath`,
`commandId`, and `appId`/`appName` inside `target` — except the top-level
`plan_id`. Result fields are snake_case — `app_id`, `deploy_hash`, `command_id`,
`deploy_id`, `lifecycle_revision`, `blobs_uploaded`, `blobs_deduped`. Each tool
accepts and returns only the spelling shown here.

Every successful result is a JSON text block. A field is present even when its
value is `null`; `null` is a JSON null, not an omitted key. Timestamps are
strings passed through from the control plane, not numbers, and this page does
not pin their layout.

### `list_apps`

Lists the app records the control plane returns for the token. The MCP server
performs no filtering of its own, so the visibility rule is exactly the control
plane's authorization for that token. Takes no arguments. Each entry carries:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Canonical `app_...` id. |
| `name` | string | The app's routable name. |
| `deploy_hash` | string or null | Current deploy's content hash; `null` before the first deploy. |
| `archived_at` | string or null | Timestamp while archived; `null` while active. |

The entries are enough to pick a target and are not the full app record:
`plan_id` is omitted.

### `get_app`

Returns one app by `target`. The result carries:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Canonical `app_...` id. |
| `name` | string | The app's routable name. |
| `plan_id` | string | The app's plan id, for example `free`. |
| `deploy_hash` | string or null | Current deploy's content hash; `null` before the first deploy. |
| `archived_at` | string or null | Timestamp while archived; `null` while active. |
| `created_at` | string | Timestamp of creation. |
| `updated_at` | string | Timestamp of the last record change. |

### `create_app`

Creates an app. Arguments:

- `name` — the app name, required and non-empty at the MCP layer. The control
  plane enforces the full rule: 1-64 characters drawn from ASCII letters,
  digits, `-` and `_`; not a name the platform edge already routes; and not one
  whose first four characters are `app_` (case-insensitive), which is reserved
  for app ids. A name outside the charset or length is `400`; a well-formed but
  unavailable name — an edge-routed label or an `app_`-prefixed one — is `409`.
  The platform edge reserves `auth`, `api`, `console` and `control`. See
  [Errors](#errors).
- `plan_id` — optional. Defaults to `free`. The MCP server does not validate it;
  the control plane refuses an unknown or archived plan with `400`, so the
  assignable values are whatever that deployment's plan catalog holds.

The result is the created app in the `get_app` shape. There is no argument for
the execution zone; see [Limits and gaps](#limits-and-gaps).

### `deploy_app`

Deploys a local `.zship` to an app. A `.zship` is the content-addressed deploy
artifact a build pipeline emits; this server reads its bytes and does not unpack
or validate it. Arguments:

- `target` — the app to deploy to.
- `zshipPath` — path to the local `.zship` artifact. The server process reads
  the file itself, so a relative path resolves against the server process's
  working directory, not the MCP client's project root. Use an absolute path
  when the directory the client launched the server in is not certain.
- `commandId` — optional. Resume a deploy whose outcome was not reported. A
  string that is not a canonical `dcm_...` id is refused before any request is
  sent, with the reason `commandId must be a canonical deploy command id`.

With a **name** target, the app is created first when the name does not exist;
the new app lands on the free plan and its details come back in `created`
(`null` when the app already existed). With an **id** target the app must
already exist.

The result carries:

| Field | Type | Meaning |
| --- | --- | --- |
| `app_id` | string | Canonical `app_...` id deployed to. |
| `created` | object or null | The created app in the `get_app` shape when a name target created one; `null` otherwise. |
| `command_id` | string | Canonical `dcm_...` deploy command id. |
| `deploy_id` | string | Canonical `dep_...` deployment id. |
| `deploy_hash` | string | The deployed artifact's content hash: 64 lowercase hex characters (a bare sha256 digest, no `sha256:` prefix). |
| `blobs_uploaded` | number | Artifact blobs this command wrote to the blob store. |
| `blobs_deduped` | number | Artifact blobs already present and reused. |
| `lifecycle_revision` | number or null | The revision that activated the deploy; `null` when the app is archived and the deploy is staged. |
| `replayed` | boolean | `true` when this answers a repeat of a command the control plane already accepted. |

**Resuming an unknown outcome.** Each call without `commandId` is a new deploy
command, even for the same artifact. When a call reports that the deploy outcome
is unknown, its error names a `command_id`; call `deploy_app` again with that
`commandId` and the same `zshipPath`. Control answers an exact repeat with the
original acceptance and `replayed` is `true`. A command's identity is its id
together with the artifact bytes, the app and the caller: the same id with
different bytes, another app or another caller is a `409`. See
[Deploys](control.md#deploys) for the full idempotency contract.

### `app_logs`

Reads recent worker logs for an app. Arguments: `target`, and an optional
`limit`.

- `limit` is a positive integer with no upper bound imposed by the MCP server.
  The server always fetches the full recent set from the control plane and then
  returns the last `limit` entries of that array — it does not request fewer
  lines. Omit it for the full recent set.

The result carries `app_id` (the canonical `app_...` id) and `logs` (an array of
log-line strings). A control plane that cannot reach any worker answers `502`
with the generic error `internal error`.

### `archive_app`

Archives an app by `target`: it stops serving while its record, history and data
are preserved. The result is the app in the `get_app` shape, with `archived_at`
set to a timestamp. Archive does not terminate a request already in flight, and
the archived record still holds the app's name.

### `restore_app`

Restores an archived app by `target`. The result is the app in the `get_app`
shape, with `archived_at` `null`. A deploy staged while the app was archived is
activated by the restore. The MCP result reports the app record only and carries
no lifecycle revision, so it does not by itself prove the staged deploy is
serving; see [App archive lifecycle](control.md#app-archive-lifecycle) for what
restore activates.

Both operations are idempotent at the control plane and safe to send again. The
full lifecycle — what archive keeps, what a delete would end, and why — is in
[App archive lifecycle](control.md#app-archive-lifecycle). There is no `delete`
tool.

## Errors

Two shapes of failure exist, and they are told apart by the first words of the
result text.

**Tool errors.** A tool that runs and fails returns an MCP error result
(`isError: true`) whose text begins `Error:`. The cases a creator will meet:

| Case | Result text |
| --- | --- |
| Missing token | ``Error: set ZEROSHIP_TOKEN (run `zeroship login`)`` |
| Control plane refused or failed the call | `Error: control plane returned HTTP <status>: <message>` |
| Unknown deploy outcome | `Error: the deploy outcome is unknown; call deploy_app again with commandId "<dcm_...>" and the same zshipPath to resume it without deploying twice` |
| Unknown app name | ``Error: app `<name>` not found`` |

When the unknown-outcome failure carried an underlying cause, it is appended to
`unknown` in parentheses, so the text reads `... is unknown (<cause>); call
deploy_app again ...`.

**Argument errors.** An argument set the schema refuses returns a tool error
whose text begins `Input validation error: Invalid arguments for tool <tool>:`
followed by the reason. The reasons you can branch on are the exact strings
`appId must be a canonical AppId` and `commandId must be a canonical deploy
command id`; a malformed `target` or a removed bare `app` argument fails the
same way. No control-plane request is sent for any of these.

For a control-plane failure, `<message>` is the refusal's own text: the `error`
field of the response body when it has one, else its `message`, else the HTTP
reason phrase. It is often a stable slug, so the result reads
`Error: control plane returned HTTP 403: forbidden`. Branch on the status and
the slug, not on the sentence around them. The statuses reachable through these
tools:

| Status | `<message>` | Meaning |
| --- | --- | --- |
| `400` | `name must be 1-64 alphanumeric/hyphen/underscore` | `create_app` name is outside the charset or length. |
| `400` | `unknown plan '<plan_id>' (not in the plan catalog)` | `plan_id` names no plan. |
| `403` | `forbidden` | The token is not authorized for the app or operation. |
| `404` | `app not found` | The id target names no app the token can reach. |
| `409` | prose naming the reserved label, or the `app_` prefix | `create_app` name is well-formed but unavailable. |
| `409` | `resource already exists` | `create_app` name is already taken. |
| `409` | `idempotency_key_conflict` | A `commandId` was reused with different bytes, app or caller. |
| `409` | `schema_not_applied` | The artifact's migrations have not been applied to the app's database. |
| `413` | `deploy too large` | The `.zship` exceeds the size cap. |
| `500` | `internal error` | Other infrastructure failure. |
| `502` | `internal error` | `app_logs` could not reach any worker. |
| `503` | `internal error` | The deploy blob store is unavailable; retry the deploy. |

Every `5xx` above returns the same generic body, and the status plus the call you
made is what distinguishes them. The MCP result surfaces only `<status>:
<message>`. Any `detail`, `remedy`, `cap_bytes` or `trace_id` the control plane
put in the response body — including the `trace_id` on every `5xx` — is not
passed through to the tool result.

The server does not retry a call on its own; resuming an unknown deploy is an
explicit second call. The full control-plane error contract is in
[`@zeroship/control`](control.md#errors).

## Limits and gaps

The MCP surface is seven tools over the app lifecycle. It does not cover the
rest of the control plane:

- **No execution zone.** An **execution zone** is an operator-declared set of
  worker deployment units that share creator-side connectivity; an app's zone is
  chosen when it is created and frozen there. `create_app` takes no zone. A
  deployment that declares exactly one zone is fine, because the control plane
  defaults to it; one that declares several refuses a create that names none,
  with `400` and a message naming the zones. The MCP server has no way to name
  one, so in that deployment create the app first through the CLI or
  [`@zeroship/control`](control.md#an-apps-execution-zone-is-chosen-once), then
  deploy to it by id.
- **No environment, secrets, egress rules, organizations, projects, plans or
  usage.** Those remain CLI and [`@zeroship/control`](control.md) calls.
- **No delete.** `archive_app` is the terminal lifecycle action here; ending an
  app is a CLI or control-client call.
- **Reduced records.** `list_apps` omits `plan_id`, and no tool returns usage or
  billing. Read those from the control client.
