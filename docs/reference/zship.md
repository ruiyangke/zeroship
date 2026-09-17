# `.zship`

A `.zship` file is the deploy artifact for one app. `pnpm build` writes it to the
path `build.output` names (default `dist/app.zship`), and `zeroship deploy`
uploads it to the platform.

The archive is a zstd-compressed tar with a fixed layout: `manifest.json` first,
then one `blobs/<sha256>` entry per distinct file. Every blob is named by the
SHA-256 of its bytes, so identical files are packed once and the manifest
references each by that name. The artifact is not opaque — standard tools read it
(see [Inspecting an artifact](#inspecting-an-artifact)).

## What the artifact carries

- **The compiled application** — the server modules your code becomes, each
  content-addressed, plus a map of the static files the build emitted.
- **The routing manifest** — one entry per URL path and per RPC procedure, with
  the policy the gateway applies before dispatch.
- **The generated database schema descriptor** — the schema the runtime installs
  for `env.db`. Migration documents are not carried.
- **Workflow declarations and schedule registrations** discovered at build time.
- **Declared outbound-request hints and custom OAuth scopes.**
- **Build metadata** — the compiler identifier and the build timestamp.

It does **not** carry:

- **Migration documents.** They apply through the migration service
  (`zeroship migrate`), not from the artifact.
- **Your project config** (`zeroship.jsonc`), which never leaves your machine.
- **Dev-only surfaces** — the dev auth provider and Vite build metadata are
  absent from a production artifact.

## Manifest version

The manifest schema version is `1`. A manifest that declares any other version is
refused at deploy with `400` and an `unsupported manifest version` error.

## Current manifest shape

`manifest.json` is a single object. Its fields:

| Field | Meaning |
| --- | --- |
| `version` | Manifest schema version; `1`. |
| `runtime_date` | The project's runtime date, copied from `zeroship.jsonc`. Inert: the platform transports it but does not change behavior on it. |
| `deploy_hash` | The deployment identity the platform assigns on ingest. Absent from a freshly built artifact. |
| `worker` | The compiled server code: an `entry` specifier plus a `modules` map of specifier to blob hash. Absent for a static-only app. |
| `resources` | The routing map. Keys are `/<path>`, `rpc:<id>` or `*`; each value is a resource entry (see [Resource entries](#resource-entries)). |
| `schemas` | JSON Schemas keyed by `"sha256:<64-hex>"`, referenced by a resource entry's `input_schema` / `output_schema`. |
| `aliases` | Wire-id map from `<filePath>::<exportName>` to `"rpc:<id>"`. Parsed and ignored by the gateway; the build does not write it (see [RPC resources](#rpc-resources)). |
| `transformer` | The RPC wire transformer; `json`. |
| `assets` | Build-time static files: URL path to content hash, content type and size. Immutable until the next deploy. |
| `runtime_assets` | Assets the running app added. Empty in a freshly built artifact. |
| `asset_version` | Counter bumped when `runtime_assets` changes; `0` in a freshly built artifact. |
| `sourcemaps` | Asset hash to sourcemap blob hash, when sourcemaps are built. |
| `auth` | The app's declared custom OAuth scopes, each `{ id, label, description }`. Omitted when none are declared. |
| `net` | Outbound-request hints for review. Not a grant. |
| `metadata` | `compiler` and `built_at`. |
| `schedules` | Workflow schedule registrations discovered at build time. Each carries `name`, `workflowName`, `input`, `overlap`, `catchUp`, and a timing `schedule`. |
| `workflows` | The workflow declarations discovered at build time. A workflow not declared here cannot be started. |
| `runtime_descriptor` | The generated database schema descriptor, carried as a content-addressed blob. Absent for a schema-less app. |

A schedule registration's `schedule` object carries timing only: `kind: "cron"`
with `cron_expr` and `tz`, or `kind: "interval"` with `interval_ms` (positive
milliseconds) and `anchor` (`"epoch"` or `"deploy"`). The `overlap` and `catchUp`
policies sit on the registration, not inside `schedule`: `overlap` is `"allow"`
or `"skipIfRunning"`, and `catchUp` is `{"mode":"skip"}` (the default) or
`{"mode":"backfill","max":N}`. `input` is the workflow's start input and
defaults to `{}` when the source declares none.

`worker`, when present, is:

```json
{
  "entry": "index.js",
  "modules": { "index.js": "<64-hex sha256>" }
}
```

`entry` names a key in `modules` — it is not resolved relative to anything —
and each `modules` value is a `blobs/<sha256>` reference. A manifest whose
`entry` is not a key in `modules` is refused at ingest.

`resources` is the routing source of truth. The build populates it with the asset
prefix, common public files, prerendered HTML routes, a catch-all, and one entry
per RPC procedure. `assets` holds build-time files; `runtime_assets` holds files
the app adds at runtime; `asset_version` tells the gateway when to resync the
runtime set.

### Resource entries

A resource entry mixes at most one routing action with any number of policy
fields, all `snake_case` on the wire. Optional fields are omitted when absent,
so a minimal entry is `{}`.

Routing actions — at most one per entry:

- `redirect` — outward hop: `{ "to": <url>, "status": <3xx, default 302> }`.
- `static` — serve an asset: `{ "try": [ <asset path>, ... ] }`, resolved
  left-to-right, first hit wins.
- `rewrite` — declared but not implemented; a manifest that declares one is
  refused at deploy, so use `redirect` or `static` instead.

Policy fields — any combination:

- `auth` — the principal a request must present: `"anonymous"` or `"user"`.
- `publicly_accessible` — boolean; required `true` when `auth` is `"anonymous"`,
  otherwise the deploy is refused.
- `rate_limit` — `{ "rpm"?, "rps"?, "per" }`, where `per` is `"ip"`, `"user"`,
  `"session"` or `"app"`.
- `cors`, `cache`, `csrf_origins`, `idempotent`, `idempotency_ttl_hours`,
  `max_input_bytes`, `middleware` — per-resource request controls.
- `required_scopes` — OAuth scopes a signed-in user must carry to reach the
  resource.
- `override` — inherited field names this entry deliberately re-declares.

Procedure-only metadata, on `rpc:` keys:

- `kind` — `"query"`, `"mutation"`, `"action"`, `"stream"` or `"subscription"`.
- `input_schema` / `output_schema` — `"sha256:<64-hex>"` keys into `schemas`.

Effective policy inherits along a chain: a `rpc:` key inherits from its
dot-segment family (`rpc:todos.create` from `rpc:todos`, and both from `*`), a
path key from its path-segment ancestors, the same way as the authoring surface
described in [`@zeroship/rpc`](rpc.md). The gateway flattens the chain once when
it loads the app; it is not walked per request.

## Limits

Ingestion enforces these caps on every deploy:

| Limit | Value | Refusal |
| --- | --- | --- |
| Compressed artifact size | 256 MiB | `413` |
| Decompressed archive size | 256 MiB | `413` |
| `manifest.json` size | 1 MiB | `413` |
| Single blob size | 16 MiB | `413` |
| Blobs per deploy | 10 000 | `400` |

A size refusal names the cap and the observed size. The deploy body must use
content type `application/x-zship`; anything else is `415`.

## Deployment identity

`deploy_hash` is the SHA-256 of the manifest with its own `deploy_hash` field
omitted, object keys sorted, and insignificant whitespace removed. The platform
computes it on ingest and stores the manifest with it inserted, so a freshly
built artifact carries no hash.

The identity comes from the manifest alone: two builds with equal manifests get
the same identity, and any change the manifest records produces a new one. Field
presence and extension metadata are preserved, so a stored manifest can be
verified against its identity.

## Network Requests

`net.requests` records the outbound hosts your app intends to reach:

```json
{
  "net": {
    "requests": [
      {
        "host": "api.example.com",
        "port": 443,
        "reason": "Call the upstream API"
      }
    ]
  }
}
```

The manifest never grants network access, and deploying a bundle grants nothing:
the entries are inert hints. The platform compares them against the egress rules
accepted for the app; a host/port pair with no accept rule stays blocked, so the
connect attempt is refused — the app cannot open that socket or outbound
WebSocket at all.

You write those rules yourself, out of band, with
`POST /api/apps/{app_id}/egress-rules` (`env:write` on the app; `GET` lists under
`env:read`, `DELETE` removes under `env:write`). `app_id` is the app's typed id
(`app_…`). A rule carries a required `verdict` (`"accept"` or `"reject"`), a
`destination` that is either an exact DNS name or an address range in CIDR form,
a `port`, and an optional `note` — the `POST` body is
`{ "verdict", "destination", "port", "note"? }`. Wildcards such as
`*.example.com` are refused with `400` (`invalid egress rule`). Your plan caps
how many accept rules an app may hold (`max_accept_rules`), reject rules
(`max_reject_rules`), sockets (`max_sockets`) and bytes (`egress_ceiling_bytes`);
`GET` returns each ceiling and the count in use, and exceeding a cap is `409`.
The full request/response contract is the [`@zeroship/control`](control.md)
`egressRules` namespace.

A hint can only express an accept. A bundle has no affordance for a reject, so
the reject half of the rule set is authored out of band only.

## RPC resources

RPC procedures appear in `resources` with keys of the form `rpc:<id>`. An id is
one or more ASCII letters, digits, or the characters `.`, `_`, `*`, `-`
(`[a-zA-Z0-9._*-]+`) after the `rpc:` prefix. The build derives one entry per
procedure discovered from `"use server"` modules and `@zeroship/rpc/server`
wrappers, using the procedure's pinned `id` (`fn.config.id`, or the wrapper's
`{ id: "..." }` config).

A production build fails when a procedure has no such id and its id would
default from its export name — the failure is at build time (`pnpm build`
errors), not at deploy. Every deployed RPC must pin an explicit `id`. The id is
a pure function of current source: the build carries no alias state between
builds, so renaming a procedure without a pinned id changes its wire id and
breaks deployed callers.

The request path is not stored per procedure. The gateway and worker agree on the
reserved dispatch prefix:

```text
/__zeroship/v1/<id>
```

The manifest's `transformer` names the wire format; it is `"json"`. The encoded
request/response body under that transformer is the [`@zeroship/rpc`](rpc.md)
transport shape (`{"json": …}` with a `meta` block for values plain JSON cannot
represent exactly).

## Inspecting an artifact

A `.zship` is a zstd-compressed tar, so standard tools read it.

```bash
# The whole manifest.
tar --zstd -xOf dist/app.zship manifest.json

# Every RPC the build declared, which is the usual question -
# a procedure missing here will 404 at runtime no matter what the source says.
tar --zstd -xOf dist/app.zship manifest.json | grep -oE '"rpc:[^"]+"'

# Archive layout: the manifest, then content-addressed blobs.
tar --zstd -tf dist/app.zship
```

The manifest is emitted as one line. `jq` is a convenience, not a requirement:
these commands work on a machine with no `jq` and without the zeroship CLI on
`PATH`.
