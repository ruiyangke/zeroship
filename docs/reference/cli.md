# `zeroship` CLI

The `zeroship` command is the tool you run on your own machine to sign in,
deploy a built app, apply its migrations, and manage its configuration. It does
not build: the build is `@zeroship/vite-plugin` (see [`vite-plugin.md`](vite-plugin.md)),
and the CLI uploads the `.zship` artifact that build produces (see
[`zship.md`](zship.md)). Its requests go to a **control plane**: the hosted
zeroship service that holds your app's records, provisions and operates it, and
answers the CLI's authenticated HTTP calls (see [`control.md`](control.md)).

This page assumes the `zeroship` binary is already on your `PATH`. zeroship is
pre-launch and has no published installer; the binary is built from the zeroship
repository, and the local setup is in
[`docs/runbooks/local-dev.md`](../runbooks/local-dev.md).

## Project configuration

Most commands read the project's `zeroship.jsonc` for the app, control plane,
artifact path and migration path, so you rarely pass a target. The file is read
by the CLI and by the build, never by the runtime and never packed into a
`.zship`. A minimal file:

```jsonc
{
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "my-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" }
}
```

`name`, `control`, `runtime_date`, `build` and `migrations` are required at the
root; inside `build`, `mode`, `dist` and `output` are; inside `migrations`, `dir`
and `out` are. Optional keys are `app` (the deploy target's app id, which the
first `deploy` may write back), `secrets` (secret NAMES only, never values),
`environments` (named targets selected with `--env=`), and `build.serverEntry`.
Unknown keys are refused. The full contract — defaults, patterns, environments,
the writeback and precedence — is in [`project-config.md`](project-config.md);
this page covers the commands.

## Sign in

```
zeroship login   [--control=URL] [--provider=platform|supabase]
                 [--config=PATH] [--env=NAME]
zeroship whoami
zeroship logout
```

`login` runs the OAuth 2.0 device authorization grant (RFC 8628). It prints a
verification URL and a user code, you approve in a browser, and the CLI stores
the resulting credential. It first asks control which authorization server it
trusts (RFC 9728 discovery), so the CLI cannot be pointed at an issuer whose
tokens control would reject; if control does not advertise one, login fails with
a message naming the control URL and the HTTP status. The flow asks for
`offline_access`, so the credential carries a refresh token; the CLI rotates it
automatically and writes the successor to disk before using it. `--provider`
accepts `platform` (the default) and `supabase`; both values run the same device
flow against the authorization server control advertises, so the flag does not
select a second login backend.

The credential is written to `<state>/zeroship/token.json` with mode `0600`,
where `<state>` is `ZEROSHIP_CONFIG_HOME` if set, else `XDG_CONFIG_HOME`, else
`$HOME/.config`. This is the same per-user state directory `organization use`
records into. The environment names the CLI reads are in
[`env-vars.md`](env-vars.md).

`whoami` prints the signed-in account's identity (the subject id for a platform
credential; an email when the provider supplies one) and the token's expiry.

`logout` deletes the local credential file. It performs no server-side token
revocation; the refresh family stays valid until it expires or is revoked.

Every command that needs a credential resolves it in one order: `--token=<token>`
first, then `ZEROSHIP_TOKEN`, then the credential `login` stored. A command that
finds none fails with the hint to run `zeroship login`.

## Your first deploy

Two commands, from the project directory, once `login` is done:

```
pnpm build          # @zeroship/vite-plugin writes the .zship and generated/
zeroship deploy     # create the app (first run) and upload the artifact
zeroship migrate    # apply the app's migrations to its deployed database
```

The `.zship` is the build's deploy artifact: a content-addressed archive of the
compiled app and its routing manifest (see [`zship.md`](zship.md)). The CLI
uploads it byte-for-byte and never parses or rewrites it.

The artifact path, app and control plane come from `zeroship.jsonc`, and each
command prints what it resolved and from where before it acts. Neither command
needs a target argument.

On the first deploy the file has no `app` key. `deploy` falls back to the project
`name`, creates that app, and writes its id back into `zeroship.jsonc`:

```
created app my-app (app_034klb07lrb9jgma6imvmx000)
  wrote app id into /home/me/app/zeroship.jsonc
```

That id is an `app_...` identity. zeroship ids are prefixed by entity: `app_`
an app, `dep_` a deployment, `dcm_` a deploy command, `org_` an organization,
`prj_` a project, `ivt_` an organization invitation, `usr_` a user. Each is an
opaque value; pass it back unchanged.

Only `deploy` takes the `name` fallback. `migrate`, `secret` and `var` require an
app id, which is why `deploy` runs first. The writeback and its limits — an
explicit `app`, an `--app`, or an `--env=` deploy is never written — are in
[`project-config.md`](project-config.md).

**Order matters for an app with a database.** `deploy` ships code and never
touches the database; `migrate` creates the schema, its tables, and the runtime
database access that `env.db` depends on. For a brand-new database app the first
`deploy` is refused with `409 schema_not_applied`, then `migrate` applies the
schema, then `deploy` again goes live. The refusal and its remedy are in
[`db.md`](db.md).

## `zeroship deploy`

Uploads a `.zship` to the control plane.

```
zeroship deploy [<path-to-.zship>] [--app=<id>] [--app-name=<name>]
                [--control=URL] [--token=<token>] [--no-create]
                [--command-id=<id>] [--config=PATH] [--env=NAME]
```

- **The path** is optional. With no positional it is `build.output` from
  `zeroship.jsonc`; with no file at all, the path is required.
- **`--app=<id>`** addresses the app by its `app_...` identity. An id that
  resolves to nothing is an error: deploy never creates an app from an id.
- **`--app-name=<name>`** addresses the app by its routing label (the hostname
  subdomain). A name that matches nothing is created on first deploy, unless
  `--no-create` is given.
- **`--no-create`** refuses to create a missing app and keeps the original
  failure: `app <name> not found; remove --no-create to create it on first
  deploy`.
- **`--command-id=<dcm_...>`** resumes a deploy whose outcome was not reported.
  Each invocation otherwise mints a new command id and prints it.
- **`--env=<name>`** selects an `environments` entry, which supplies that
  target's `app` and `control` (see [`project-config.md`](project-config.md)).

`deploy` checks the app's declared secret names (the `secrets` array in
`zeroship.jsonc`) against what the app has configured and warns once for each
declared name that is missing. It never reads a secret value. The check is
advisory: a failure to run or parse it prints `zeroship deploy: warning: could
not check declared secrets for app <id>: <reason>` and the upload continues.

A deploy whose generated schema descriptor (the schema the build recorded for
`env.db`) disagrees with the app's newest applied migration is refused with
`409 schema_not_applied`, and nothing goes live. When a migration set exists
beside the project config, deploy prints a reminder naming `zeroship migrate`
before uploading. That reminder is a hint, not a verdict: it fires on the
presence of the migration set, not on whether the migrations are already applied.

When control does not answer an attempt, or answers `5xx`, the outcome is
unknown. Deploy resends the same bytes with the same command id (three attempts,
waiting one second before the second and doubling each later wait), and control
answers a repeated command with its original result.
If no attempt is answered, the CLI prints the command id and tells you to resume
with `--command-id=<dcm_...>`; a deploy is never published twice.

On success deploy prints the `deploy_id`, the `deploy_hash` (the deployment's
identity), and the blob counts (files uploaded and files already stored). An
archived app accepts a deploy but stages it; the deployment becomes live when
the app is restored.

## `zeroship migrate`

Applies the app's committed migrations to its deployed database.

```
zeroship migrate [<path-to-migrations.ir.json>] [--app=<id>] [--app-name=<name>]
                 [--control=URL] [--token=<token>] [--config=PATH] [--env=NAME] [--yes]
```

- **The path** is optional. With no positional it is
  `<migrations.out>/migrations.ir.json` from `zeroship.jsonc`.
- **`--app`** takes the app's id; **`--app-name`** its routing label. Unlike
  deploy, migrate never creates an app: a name that matches nothing fails with
  `app <name> not found; zeroship migrate never creates an app - deploy it
  first, or pass its id with --app=`.
- **`--yes`** is required to migrate an environment marked `"protected": true`
  (an `environments.<name>` member).

The file posted is the build's recorded migration set (its intermediate
representation, or IR); the CLI does not parse or rewrite it. `migrate` prints
what it resolved, how many operations it applied and skipped — the
`applied`/`skipped` counts are operations, not migration files — and the
migration id.

## `zeroship secret` and `zeroship var`

Both commands manage per-app configuration. `secret` values are encrypted at
rest; `var` values are plaintext. Both require `--app=<id>` (or an `app` in
`zeroship.jsonc`); neither accepts a name. In your app, `env` is the runtime
object exposing these values (`env.KEY`), `env.db` is its managed database, and
`process.env` is the Node-style environment any bundled npm dependency can read.

```
zeroship secret set KEY=value --app=<id> [--expose]
zeroship secret list --app=<id>
zeroship secret rm KEY --app=<id>
zeroship secret expose KEY --app=<id>
zeroship secret unexpose KEY --app=<id>
zeroship secret expose-list --app=<id>

zeroship var set KEY=value --app=<id>
zeroship var list --app=<id>
zeroship var rm KEY --app=<id>
```

A key is 1 to 64 bytes: an uppercase ASCII letter followed by uppercase letters,
digits or underscores.

A secret is always readable as `env.KEY`. It reaches `process.env` only when
exposed (`--expose` on `set`, or `secret expose KEY`), because any bundled npm
dependency can read `process.env`. A var is always visible in both `env` and
`process.env`, so never put a credential in a var. Which surface wins on a name
collision, and the full read contract, are in [`env-vars.md`](env-vars.md).

`secret expose` and `secret unexpose` read the current list and replace it, so
exposing one secret does not disturb the others. `secret expose-list` prints the
exposed names. `rm` on a name the app does not have exits non-zero.

## `zeroship config`

```
zeroship config show [--config=PATH] [--env=NAME]
zeroship config path [--config=PATH]
```

`config show` prints the resolved `zeroship.jsonc` as canonical JSON (object keys
sorted, no whitespace), after any `--env` overlay. It answers "which app and
which control plane is this directory pointed at". It does not show flag or
environment-variable overrides; those appear in the provenance lines the
operating commands print. `config path` prints the file that would be read.

See [`project-config.md`](project-config.md) for the file itself, its keys, and
its precedence rules.

## `zeroship serve`

Runs an app deployment or a single JavaScript file against the local runtime.
This is the dev-tier runtime; `pnpm dev` (through `@zeroship/vite-plugin`) spawns
it, so you normally run it directly only to serve a `.zship` or a standalone
file.

```
zeroship serve <app.zship|file.js> [--port=3000] [--workers=0]
               [--cpu-limit=MS] [--wall-timeout=MS] [--heap-limit-mb=MB]
               [--workflow-config=PATH]
```

Flag defaults: `--port` is `3000`, `--workers` is `0`, and `--heap-limit-mb` is
`512` (`ZEROSHIP_HEAP_LIMIT_MB` overrides it when the flag is absent).
`--cpu-limit` and `--wall-timeout` have no CLI default; the runtime's own limits
apply. The dev server's limits and defaults differ from a deployed app's; see
[`runtime-limits.md`](runtime-limits.md).

It uses a project-local SQLite database and local key-value store under
`.zeroship/` unless `DATABASE_URL` selects another backend. `DATABASE_URL` is a
connection URL; with none set, the database is `sqlite:.zeroship/dev.sqlite`.
Dev metering accumulates counters that are not readable locally. Local values
arrive through `.env` and the `ZS_VAR_` prefix: a `.env` value named
`ZS_VAR_<NAME>` reaches the app as `env.<NAME>`, with the prefix stripped; every
other variable stays in `process.env` only. See [`env-vars.md`](env-vars.md).

## `zeroship organization`

An organization owns projects, a project owns apps, and the organization is the
party billed. The app your `zeroship.jsonc` names belongs to one of your
projects; `deploy` and `migrate` act on that app. A user can belong to several
organizations, so most subcommands act on a selected one.

```
zeroship organization create <name> [--slug=SLUG] [--billing-email=ADDR]
zeroship organization list
zeroship organization show [--organization=org_...]
zeroship organization use <org_...> | --clear
zeroship organization members [--organization=org_...]
zeroship organization invite <email> --role=ROLE [--organization=org_...]
zeroship organization revoke <ivt_...> [--organization=org_...]
zeroship organization join <token>
zeroship organization role <user-id> --role=ROLE [--organization=org_...]
zeroship organization remove <user-id> [--organization=org_...]
zeroship organization leave [--organization=org_...]
zeroship organization transfer <user-id> [--organization=org_...]
zeroship organization dissolve [--organization=org_...]
zeroship organization projects [--organization=org_...]
```

`use <org_...>` records the selection in the CLI's state directory (beside
`token.json`), so the other subcommands need `--organization=` only to override
it. `use` touches no server and needs no credential; `--clear` forgets the
selection.

`create` and `join` select the new organization when nothing is selected; if a
selection already exists they leave it alone and print how to switch. `invite`
emails the invitation and prints the token once; the token is stored only as a
digest, so a lost one is revoked and re-issued. `leave` gives up your own seat
and needs no rank; `remove` takes someone else's and needs authority over them.
`dissolve` closes the organization for everyone, is owner-only, is refused while
any project remains, and cannot be undone.

`--role` takes one of the platform's role names: `viewer`, `developer`,
`billing`, `admin` and `owner`. The CLI passes the value through and does not
validate it; the control plane refuses an unknown role and names the full list.
A project seat narrows: a member's authority on a project is the lower of their
organization rank and their project rank, and organization admins reach every
project without a seat.

`projects` manages the projects themselves (create, rename, delete, members,
add, role, remove). A bare `zeroship organization projects` lists the
organization's projects, and `zeroship organization` with no subcommand prints
the full verb list.

## Platform operation is not here

`zeroship dev init` and `zeroship join-token` configure a zeroship deployment
itself — the platform's own secrets, and the credentials workers use to join it.
They are not part of building or deploying an app. They are covered in the
runbooks ([`local-dev.md`](../runbooks/local-dev.md),
[`worker-join-signers.md`](../runbooks/worker-join-signers.md)).

## Flags and precedence

Write a flag's value after `=`: `--app=<id>`, `--token=<token>`,
`--control=<url>`. Some flags also accept a following word, but not all do, so
the `=` form is the one to use. The `--token` value is read only from the `=`
form; the space form is ignored and falls through to `ZEROSHIP_TOKEN` or the
stored credential.

An unknown `--flag` is refused rather than ignored, because a typo'd `--control`
would otherwise fall back to the default control plane in silence.

Target precedence, per [`project-config.md`](project-config.md):

```
flag  >  env var  >  zeroship.jsonc (--env)  >  zeroship.jsonc  >  built-in default
```

The built-in default applies only when there is no `zeroship.jsonc`. With a file
present and a key the command needs absent, the command fails naming the key
instead of guessing.

## Output and exit codes

Data goes to stdout; progress, warnings and errors go to stderr, so a command's
result can be piped. A successful command exits `0`; a command that fails exits
non-zero with a `zeroship <command>: <message>` line on stderr. The CLI defines
no numeric error codes; when a failure comes back from the platform, the message
carries the HTTP status and the server's response body verbatim, which is where
codes such as the `409 schema_not_applied` refusal on deploy come from.

`zeroship` with no command, and an unrecognized top-level command, print the
command list. There is no `zeroship build`.

## See also

- [`project-config.md`](project-config.md) — `zeroship.jsonc`, the writeback, precedence
- [`zship.md`](zship.md) — the artifact `deploy` uploads
- [`control.md`](control.md) — the HTTP API the commands call
- [`env-vars.md`](env-vars.md) — `ZEROSHIP_TOKEN`, `ZEROSHIP_CONTROL_URL`, the
  state directory
- [`db.md`](db.md) — migrate-before-deploy and the `409` refusal
- [`vite-plugin.md`](vite-plugin.md) — the build and the dev runtime
- [`auth.md`](auth.md) — sign-in and sessions
- [`runtime-limits.md`](runtime-limits.md) — the dev server's limits
