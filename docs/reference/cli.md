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
  "databases": {
    "main": {
      "id": "dbs_03evr3oqx1200yyd6zj2cebfw",
      "migrations": "migrations",
      "out": "generated/zeroship"
    }
  },
  "apps": {
    "app": { "databases": ["main"], "primary": "main" }
  }
}
```

`name`, `control`, `runtime_date`, `build`, `databases` and `apps` are required
at the root; inside `build`, `mode`, `dist` and `output` are; inside a database
entry, `id`, `migrations` and `out` are; inside an app entry, `databases` is.
The keys of `databases` and `apps` are LOCAL LABELS: they name an entry in this
file, the CLI dereferences one to an id before any request, and a label never
travels as an identifier. Optional keys are an app's `app` (its id, which the
first `deploy` may write back) and `primary` (required once it uses a
database), `secrets` (secret NAMES only, never values),
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
zeroship migrate    # apply the database's migrations to it
```

The `.zship` is the build's deploy artifact: a content-addressed archive of the
compiled app and its routing manifest (see [`zship.md`](zship.md)). The CLI
uploads it byte-for-byte and never parses or rewrites it.

The artifact path, app and control plane come from `zeroship.jsonc`, and each
command prints what it resolved and from where before it acts. Neither command
needs a target argument.

On the first deploy the selected `apps` entry has no `app` id. `deploy` falls
back to the workspace `name`, creates that app, and writes its id back into
that entry in `zeroship.jsonc`:

```
created app my-app (app_034klb07lrb9jgma6imvmx000)
  wrote app id into /home/me/app/zeroship.jsonc
```

That id is an `app_...` identity. zeroship ids are prefixed by entity: `app_`
an app, `dbs_` a database, `dep_` a deployment, `dcm_` a deploy command,
`org_` an organization, `prj_` a project, `ivt_` an organization invitation,
`usr_` a user. Each is an opaque value; pass it back unchanged.

Only `deploy` takes the `name` fallback. `secret` and `var` require an app id,
which is why `deploy` runs before them. `migrate` does not: it addresses a
database, so it needs no app and may run before any deploy at all. The writeback
and its limits — an entry that already carries an id, an `--app-name`, or an
`--env=` deploy is never written — are in
[`project-config.md`](project-config.md).

**The two commands are independent for an app with a database.** `deploy` ships
code and never touches the database; `migrate` creates the schema and its
tables in the database you name, whether or not an app has been deployed. Run
them in either order — deploy does not check that you migrated, so a build
reaching a column the database lacks fails at query time naming that column.
What deploy DOES refuse is a database the app holds no live binding to: create
the database with [`zeroship db`](#zeroship-db) and bind the app to it before
the first deploy (see also [`db.md`](db.md)).

## `zeroship deploy`

Uploads a `.zship` to the control plane.

```
zeroship deploy [<path-to-.zship>] [--app=<label>] [--app-name=<name>]
                [--control=URL] [--token=<token>] [--no-create]
                [--command-id=<id>] [--config=PATH] [--env=NAME]
```

- **The path** is optional. With no positional it is `build.output` from
  `zeroship.jsonc`; with no file at all, the path is required.
- **`--app=<label>`** names one of the `apps` entries the file declares, and
  the id comes from the file — so a label never travels as an identifier and a
  typo lists the labels that exist. A workspace declaring one app implies it;
  one declaring several requires the flag. With NO file there are no labels, so
  the same flag is an `app_...` id; which case you are in is decided by whether
  there is a file, never by inspecting the value. An id that resolves to
  nothing is an error: deploy never creates an app from an id.
- **`--app-name=<name>`** addresses the app by its routing label (the hostname
  subdomain). A name that matches nothing is created on first deploy, unless
  `--no-create` is given.
- **`--no-create`** refuses to create a missing app and keeps the original
  failure: `app <name> not found; remove --no-create to create it on first
  deploy`.
- **`--command-id=<dcm_...>`** resumes a deploy whose outcome was not reported.
  Each invocation otherwise mints a new command id and prints it.
- **`--env=<name>`** selects an `environments` entry, which supplies that
  target's app ids, database ids and `control` (see
  [`project-config.md`](project-config.md)).

`deploy` checks the app's declared secret names (the `secrets` array in
`zeroship.jsonc`) against what the app has configured and warns once for each
declared name that is missing. It never reads a secret value. The check is
advisory: a failure to run or parse it prints `zeroship deploy: warning: could
not check declared secrets for app <id>: <reason>` and the upload continues.

A deploy whose artifact declares a database the app holds no live binding to is
refused with `409 database_not_bound`, and nothing goes live; the body names
the call that grants one. Deploy compares no schema — an equality test would
make one app's migration invalidate the build of every other app on the same
database. So a deploy whose migrations are unapplied is accepted, and the first
query against a missing column fails at query time. When a migration set exists
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

Applies a database's committed migrations to that deployed database.

```
zeroship migrate [<path-to-migrations-dir>] [--database=<label>]
                 [--control=URL] [--token=<token>] [--config=PATH] [--env=NAME] [--yes]
```

**A migration is not an app operation.** The target is the DATABASE, and it is
authorized at the project that owns it — a qualifying project seat at developer
rank, the same authority that created the database. No app is named, no app is
needed, and there is no ordering constraint against `zeroship deploy`: a
database `zeroship db create` left `active` with no app bound to it is a legal
target, and this is the command that fills it.

- **The path** is optional, and it names the DIRECTORY holding your
  `migrations/*.ts`. With no positional it is the `migrations` of the selected
  database, from `zeroship.jsonc`. A path to a file is refused: there is no
  pre-built migration file to pass, and nothing writes one.
- **`--database`** names one of the labels `zeroship.jsonc` declares under
  `databases`. A workspace declaring exactly one implies it; a workspace
  declaring several is asked which rather than guessed at, the same rule
  `--app` follows for apps. Any declared database may be migrated — `primary`
  decides which handle is `env.db`, not which schema a migration set can reach,
  and a database no app uses is addressable too. The label is dereferenced to
  its `dbs_` id before the request, so the word never travels. With no config
  file in the directory there are no labels and `--database` is the id itself.
- **`--yes`** is required to migrate an environment marked `"protected": true`
  (an `environments.<name>` member).

`migrate` reads your `migrations/*.ts` and records them into the request it
posts. That recording is the same one the build runs, so it needs Node and your
app's installed dependencies - the ones that already build your `.zship`. Run
`migrate` from a checkout with `node_modules` present, not from a machine that
only holds the built bundle.

Nothing is written to your project. The recorded set exists only in the request,
which is why the database rides in the URL rather than in the body. `migrate`
prints the migrations directory it resolved, the database it resolved, how many
operations it applied and skipped — the `applied`/`skipped` counts are
operations, not migration files — and the migration id.

A migration that fails to record stops the command before anything is applied,
and the message names the file and what the DSL refused.

An apply your seat on the database's project does not cover is refused by the
migration service with `403 forbidden`, before any document is read.
`zeroship db list --project=prj_...` shows the databases a project holds.

## `zeroship db`

Creates the databases an app uses, and grants an app access to one.

```
zeroship db create   <name> --project=prj_...
zeroship db list     --project=prj_...
zeroship db bind     <label> --app=<label> --capability=<capability>
zeroship db unbind   <label> --app=<label>
zeroship db bindings <label>
zeroship db delete   <label> [--yes]
```

A database belongs to a **project**, not to an app. It outlives the apps that
use it, and several apps in the same project may share one.

- **`<label>`** names a `databases` entry of `zeroship.jsonc`, which this
  command dereferences to its `dbs_` id before it makes any request. Two
  workspaces may both call a database `main`, so the word never travels; what
  travels is the id under it, and `--env=<name>` moves that id without moving
  the label. With no config file in the directory there are no labels and the
  argument is the id itself. `--app` follows the same rule against `apps`.
- **`create`** prints the new database's `id`. Record it in `zeroship.jsonc`
  under a label of your choosing, name that label in the app's `databases`, and
  bind it. The `name` you pass is display text for a listing; nothing resolves
  a database by it, and a name starting with `dbs_` is refused so a name and an
  id can never be mistaken for one another.
- **`bind`** is what grants access. Declaring a database in `zeroship.jsonc`
  grants nothing: `zeroship deploy` refuses an app whose manifest names a
  database it holds no active binding to. `--capability` is passed through to
  the control plane, which names the accepted set if you pass one it does not
  have.
- **`create` and `bind` declare and stop.** A new database is `provisioning`
  and a new binding is `pending` until the service holding that cluster's
  credential has made the cluster match. `zeroship db bindings` shows where
  each one is.
- **`delete`** is refused while any app still binds the database, and the
  refusal names them; unbind each first. It needs `--yes` when the selected
  environment is marked `"protected": true`.

Reads print the control plane's JSON on stdout, so they pipe; every
explanation goes to stderr.

## `zeroship secret` and `zeroship var`

Both commands manage per-app configuration. `secret` values are encrypted at
rest; `var` values are plaintext. Both require `--app` (or a sole app entry in
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
codes such as the `409 database_not_bound` refusal on deploy come from.

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
