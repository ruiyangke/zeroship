# `zeroship.jsonc`

`zeroship.jsonc` is a zeroship project's tooling configuration. It records the
facts that more than one tool needs and that everybody working on the project
shares: which app this directory deploys to, which control plane, where the
build writes its artifact, and where the migrations live.

It is read by exactly two readers:

- the **`zeroship` CLI** (`deploy`, `migrate`, `secret`, `var`, `config`, and
  `login` for `control` alone) - `crates/cli/src/project_config/`
- the **build toolchain** (`@zeroship/vite-plugin`, its `gen-types-all` script,
  and the `zeroship-dev-migrate` binary) -
  `sdks/vite-plugin/src/project-config/`

Both readers are generated from one JSON Schema, `schema/project-v1.json`.

## What it is not

**The config file itself is never read by the runtime and never packed into a
`.zship`.** The build copies one reserved scalar, `runtime_date`, into the
manifest, where it remains inert. No other config content reaches the machines
your app runs on. That is the scope invariant, and it decides every question
about what belongs here:

| | `zeroship.jsonc` | the `.zship` manifest |
| --- | --- | --- |
| Written by | you, plus the first-deploy `app` writeback | the build |
| Read by | the `zeroship` CLI and the build | control plane, gateway, worker |
| Travels | never leaves your machine | uploaded on every deploy |
| Describes | how the tooling operates | how the app behaves |

So a fact the gateway or the worker enforces on an end-user request is not a
`zeroship.jsonc` fact. Resource policy, RPC defaults and outbound network hints
stay in `src/server/config.ts`, where `defineApp` compiles them into the
manifest. Anything Vite alone reads (`plugins`, `resolve`, `server.watch`,
`build.rollupOptions`) stays in `vite.config.ts`.

`tests/project_config_gate.sh` enforces the invariant rather than trusting it:
it packs a fixture app whose `zeroship.jsonc` carries a sentinel, asserts the
sentinel appears nowhere in the decompressed archive bytes, and pairs that with
a control that plants the same sentinel inside `dist/` and requires it to be
found, so a search that cannot see a present sentinel cannot pass.

**It never holds a secret value.** See [Secrets](#secrets).

## Finding the file

An explicit `configPath` plugin option or `--config=<path>` CLI flag has the
highest precedence, followed by the `ZEROSHIP_CONFIG` environment variable.
When neither names a file, the tooling auto-discovers `zeroship.jsonc` in the
app root.

There are no format fallbacks - one filename, one format - and **no upward
directory walk**. A build or a deploy run in a subdirectory would otherwise pick
up a sibling app's `app` and `control` in silence, which is the cross-targeting
hazard the environments rule exists to close, arriving through the
file-location door.

A file named by an explicit option, flag, or environment variable that does not
exist is an error. Only auto-discovery is allowed to come up empty, and the two
readers then diverge on purpose:

- **the build proceeds on the schema defaults**, which is what keeps
  `zeroship()` working in a scratch directory with no file at all;
- **the CLI keeps the flag / environment-variable / compiled-fallback chain it
  always had**, so `zeroship deploy ./dist/app.zship --app=... --control=...`
  still works with no file present.

## Key reference

`type` and `default` come from `schema/project-v1.json`. **Defaults are applied
by the build side only.** The CLI has none: with a file present and a key it
reads absent, the command fails naming the key, because a CLI that guessed a
control URL would deploy or migrate somewhere else in silence. The `defaults`
column therefore says what the *build* assumes when the key is missing.

| Key | Type | Default | Read by |
| --- | --- | --- | --- |
| `$schema` | string | - | neither; an editor hint. The CLI refuses a file whose `$schema` names a different contract than the one it was built against. |
| `name` | string, `^[a-z0-9][a-z0-9-]{0,62}$` | required | CLI `deploy`, and only as the app name on a first push (see below). The build validates it and otherwise ignores it. |
| `app` | string | none | CLI: `deploy`, `migrate`, `secret`, `var`. An app id (uuid) or an app name. |
| `control` | string | required | CLI: the same four commands plus `login`. Control-plane base URL. |
| `runtime_date` | string, `^[0-9]{4}-[0-9]{2}-[0-9]{2}$` | required | build (transport only). The scaffold stamps the current UTC date; the build copies it into the manifest; the runtime ignores it. See [`runtime_date`](#runtime_date-is-transported-but-inert). |
| `build.mode` | `"full"` \| `"static"` | `"full"` | build. `"full"` emits `manifest.worker`; `"static"` is an SSG-only deploy with no worker. |
| `build.serverEntry` | string | auto-detected | build (dev server and production build). Omit for auto-detection. |
| `build.dist` | string | `"dist"` | build. The directory the client build writes and the only directory the packer walks. |
| `build.output` | string | `"dist/app.zship"` | build (writes it) and CLI `deploy` (uploads it). The resolved path cannot be the project root, an ancestor, a symlink, or an existing non-`.zship` path. |
| `migrations.dir` | string | `"migrations"` | build: the Vite build, the dev server, `gen-types-all`, `zeroship-dev-migrate`. |
| `migrations.out` | string | `"generated/zeroship"` | build (writes `env.db.ts`, `schema.runtime.json`, `migrations.ir.json` there) and CLI `migrate` (posts `<out>/migrations.ir.json`). Its resolved directory cannot contain the project root; gen-types refuses unknown existing target files and symlinks. |
| `secrets` | string[], each `^[A-Z][A-Z0-9_]{0,63}$` | `[]` | CLI `deploy` checks the declared names before upload. See [Secrets](#secrets). |
| `environments.<name>` | object | - | see [Environments](#environments). |

`name`, `control`, `runtime_date`, `build` and `migrations` are required at the
root. Inside them, `build` must state `mode`, `dist` and `output`, and
`migrations` must state `dir` and `out`; only `build.serverEntry` is optional.
Those same blocks inside an `environments` entry require nothing, because they
are overlays on a root that already stated everything. The scaffold writes every
required key explicitly rather than leaning on a default, which is what keeps
the two readers from disagreeing about a path neither one states.

Unknown keys are refused at every level, with the known set in the message.

### The keys the CLI reads

`name`, `app`, `control`, `runtime_date`, `build.output`, `migrations.dir`,
`migrations.out` and environment-only `protected` are marked `x-cli-read` in
the schema. That marking means two things: the CLI has no compiled default for
them, and the plugin's `config` escape hatch may not change them. The deny-list
is generated from the schema, so it cannot fall behind the fact it protects.

## Precedence

Two different orders, one for the file and one for the values inside it.

**The file**, as above: `configPath` / `--config=`, then `ZEROSHIP_CONFIG`, then
`zeroship.jsonc` in the app root.

**A value:**

```
flag  >  environment variable  >  zeroship.jsonc (selected environment)
      >  zeroship.jsonc (root)  >  compiled fallback (only when there is no file)
```

The compiled fallback is reachable **only when no file was found**. With a file
present, a key the command needs and the file does not state is an error naming
the key, not a fallback:

```
$ zeroship migrate
zeroship migrate: /home/me/app/zeroship.jsonc does not set `app`, and `zeroship` has no default for it.
Add it to the file (the scaffold writes every cross-tool key explicitly), or pass the matching flag. ...
```

Only two values take this path today. `--app` has no environment variable and no
fallback at all; `--control` reads `ZEROSHIP_CONTROL_URL` and falls back to
`http://localhost:9090` when there is no file. Your platform credential is not
in this file and never resolves from it: `--token=<PAT>`, then `ZEROSHIP_TOKEN`,
then the token `zeroship login` wrote to `~/.config/zeroship/token.json`.

`zeroship login` resolves `control` exactly like the other commands. It accepts
`--config` and `--env`, prints provenance, and reports file location, parsing,
and environment-selection errors instead of suppressing them.

### Provenance is printed before every mutating call

A file that silently supplies a control URL is more dangerous than a flag you
have to type, because the flag is in your shell history and the file is not in
the command. So every command that resolves a target from the file prints what
it resolved and where it came from, on stderr, before it acts:

```
$ zeroship migrate --env=prod
zeroship migrate: app = prod-app (from zeroship.jsonc environments.prod)
zeroship migrate: control = https://control.example (from zeroship.jsonc environments.prod)
zeroship migrate: migrations = generated/zeroship/migrations.ir.json
```

The source is one of `<flag> flag`, `$ZEROSHIP_CONTROL_URL`, `zeroship.jsonc`,
`zeroship.jsonc <member>` (a different member standing in for the one asked for
- today only `deploy` using `name` for `app`), `zeroship.jsonc
environments.<name>`, or `built-in default`.

## Environments

`environments` holds named deploy targets, selected with `--env=<name>` on the
CLI and the `env` option in the plugin:

```jsonc
"environments": {
  "staging": {
    "app": "stg-app",
    "control": "https://control.staging.example"
  },
  "prod": {
    "app": "prod-app",
    "control": "https://control.example",
    "protected": true
  }
}
```

**`app` and `control` are non-inheritable.** Every environment must state both;
one that omits either fails to parse. This is the rule the whole block exists
for: an environment that names a staging `control` and inherits the root `app`
targets a production app through a staging control plane, and nothing in the
command line shows it. Everything else inherits per member - `build` and
`migrations` merge member by member over the root, so an environment can set
`build.mode` alone and keep the root's `dist` and `output`. `secrets` is
replaced wholesale, not merged.

**There is no implicit environment and no `ZEROSHIP_ENV`.** You pass `--env=` or
you get the root. An environment variable that silently switched which database
got migrated is the same failure with extra steps. `--env=<name>` with no
`zeroship.jsonc` in the directory is an error, not a no-op, and a name that
matches no environment lists the ones that exist.

**`"protected": true`** makes `zeroship migrate` against that environment
require `--yes`:

```
$ zeroship migrate --env=prod
zeroship migrate: app = prod-app (from zeroship.jsonc environments.prod)
zeroship migrate: control = https://control.example (from zeroship.jsonc environments.prod)
zeroship migrate: the selected environment is marked "protected": true and this would
apply migrations to https://control.example. Re-run with --yes if that is what you meant.
```

It is the only mechanism here that stops a *correct* configuration from being
run at the wrong moment. `protected` is an environment-only key.

## Secrets

**`zeroship.jsonc` declares secret NAMES. It never holds a value.** The file is
tracked, and a plaintext secret in a tracked file is unrecoverable once
committed.

```jsonc
"secrets": ["STRIPE_SECRET_KEY", "RESEND_API_KEY"]
```

Values go where they already went:

| Value | Where it lives |
| --- | --- |
| A deployed app's secret | `zeroship secret set NAME=value` (add `--expose` to also put it in `process.env`) |
| A local dev value | `<root>/.env`, which the scaffold gitignores |
| Your platform credential | `~/.config/zeroship/token.json`, mode `0600`, written by `zeroship login` |

Two structural guards back the rule up. Every object in the schema is
`additionalProperties: false`, and the key names `password`, `token`, `secret`,
`key`, `apiKey` and `credentials` are refused **anywhere** in the tree with a
message naming `zeroship secret set` and `.env`. An array of strings has nowhere
to put a value in the first place.

Before uploading an archive, `zeroship deploy` lists the target app's configured
secret names and warns once for every declaration that is absent. It never reads
a secret value. If the advisory check cannot run or its response cannot be
parsed, deploy prints that validation failure explicitly and continues with the
upload.

## `runtime_date` is transported but inert

`runtime_date` is required and both readers validate its `YYYY-MM-DD` format.
The scaffolder writes the current UTC date, and the build copies that value
verbatim into `manifest.json`. The runtime does not branch on it, there is no
compatibility mechanism behind it, and none is scheduled. It is required from
day one so that the habit of writing it exists before the first dated behavior
gate does; a field absent from every project would make the first such change
expensive.

Leave the scaffolded value alone. It is not a version pin. Changing it changes
the artifact metadata but does not currently change runtime behavior.

## The `config` escape hatch

The plugin accepts a `config` option: a partial object shallow-merged over the
loaded file, or a function applied **after** the file loads and after
environment selection, so it sees exactly what the tooling resolved.

```ts
zeroship({
  config: (c) => ({
    ...c,
    build: { ...c.build, mode: process.env.SSG ? "static" : "full" },
  }),
})
```

**It may not change any field the CLI also reads** - `name`, `app`, `control`,
`runtime_date`, `build.output`, `migrations.dir`, `migrations.out`, or
environment-only `protected`. Trying to is an error naming the field. The
reason is structural: a `config` function runs inside Vite, and the Rust CLI
cannot execute JavaScript and never will, so an override there would put the
two tools back into the disagreement this file removes. Change those in
`zeroship.jsonc`, or use an `environments` entry and `--env=`.

Overridable: `secrets`, `build.mode`, `build.serverEntry`, `build.dist`.
The denial is on **change**, not on presence, because the idiom above spreads
`app` and `control` into its own result every time.

## The writeback, and starting a project with no app yet

`app` accepts an app id or an app **name**, and `zeroship deploy` resolves a
name against the control plane, creating the app when it does not exist
(`--no-create` turns that off). A fresh project therefore does not need an `app`
at all: the scaffold ships without one, and `deploy` falls back to `name` for
that first push, saying so in its provenance line.

```
$ zeroship deploy
zeroship deploy: app = my-app (from zeroship.jsonc name)
...
created app my-app (11111111-1111-4111-8111-111111111111)
```

**The fallback is `deploy`-only.** `migrate` does not take it - a typo'd name
would migrate a fresh empty app while the real one stayed broken - and
`secret` / `var` could not use it if they wanted to, because the control plane
parses that path segment as a uuid. Those three still error naming `app` until
the id is in the file. It is also off under `--no-create`, which is the flag
that says "do not invent an app".

`zeroship deploy` considers recording an auto-created id in exactly one
situation: the resolved file had no `app`, so deploy used its `name` fallback.
An existing config `app` or an explicit `--app` is never a writeback target,
even if that named target is auto-created. The file is not opened for writing.

Because the eligible file has no `app` member, the CLI appends it as the final
root member and reports the path it wrote. The edit uses the JSONC CST and
preserves comments, key order, interior blank lines, trailing commas, CRLF line
endings, and multibyte text. It may normalise extra blank lines immediately
after the root `{` or immediately before its `}`; no other formatting is
normalised. The CLI re-parses the complete project config before it writes.

```
created app my-app (11111111-1111-4111-8111-111111111111)
  wrote app id into /home/me/app/zeroship.jsonc
```

No other config member is ever written. In particular, `control` is never a
writeback target: a `--control=` typo becoming permanent is worse than typing
the flag twice.

**A deploy run with `--env=` never writes either.** The writeback helper only
targets the top-level member, and an id created for staging written at the root
is where every un-flagged command would then read production's - the
cross-targeting the non-inheritable rule exists to prevent, arriving through
the writeback door. So the command prints the line and says which block it
belongs in:

```
created app my-app-staging (2222...)
  add this under environments.staging in /home/me/app/zeroship.jsonc:
    "app": "2222...",
```

## `zeroship config`

```bash
zeroship config show [--config=<path>] [--env=<name>]   # resolved config, canonical JSON
zeroship config path [--config=<path>]                  # the file that would be read
```

`show` prints the resolved configuration with object keys sorted and no
whitespace, after the environment overlay:

```console
$ zeroship config show --env=prod
{"app":"prod-app","build":{"dist":"dist","mode":"static","output":"dist/app.zship"},"control":"https://control.example","migrations":{"dir":"migrations","out":"generated/zeroship"},"name":"zeroship-starter","protected":true,"runtime_date":"2026-08-14","secrets":["STRIPE_SECRET_KEY"]}
```

It answers "which app and which control plane is this directory pointed at",
which the file alone stops answering as soon as `environments` exists. It is
also what `tests/project_config_gate.sh` byte-compares against the TypeScript
reader's dump of the same file, so the two parsers cannot drift on defaults,
ignored keys, or type coercion without a gate failing.

`show` prints the file's own facts and the environment overlay. It does not
show flag or environment-variable overrides; those appear in the provenance
lines the mutating commands print.

## A worked example

`zeroship.jsonc` at the project root, committed:

```jsonc
// zeroship.jsonc - this project's tooling configuration. COMMITTED.
//
// Read by the `zeroship` CLI and by the build. The file itself is NEVER read
// by the runtime or packed. Its runtime_date is copied into the manifest and
// ignored.
//
// NO SECRETS. Declare NAMES under `secrets`; put the values in `.env` (local
// dev, git-ignored) or `zeroship secret set` (deployed).
{
  "$schema": "https://zeroship.ai/schema/project-v1.json",

  "name": "my-app",

  // An app id or name. An explicit value is never rewritten by deploy.
  "app": "my-app",
  "control": "https://control.zeroship.ai",

  // Scaffolded as the current UTC date. The runtime does not branch on it.
  "runtime_date": "2026-08-14",

  "build": {
    "mode": "full",
    "dist": "dist",
    "output": "dist/app.zship"
  },

  "migrations": {
    "dir": "migrations",
    "out": "generated/zeroship"
  },

  "secrets": ["STRIPE_SECRET_KEY"],

  "environments": {
    "staging": {
      "app": "my-app-staging",
      "control": "https://control.zeroship.ai"
    },
    "prod": {
      "app": "my-app-prod",
      "control": "https://control.zeroship.ai",
      "protected": true
    }
  }
}
```

`vite.config.ts` keeps only what varies per machine:

```ts
import { defineConfig } from "vite";
import { zeroship } from "@zeroship/vite-plugin";

export default defineConfig({
  plugins: [zeroship({ devServerPort: 3001 })],
});
```

The whole flow, with no target flags anywhere:

```bash
pnpm build                       # writes build.output and migrations.out
zeroship deploy                  # reads app, control, build.output
zeroship migrate                 # posts <migrations.out>/migrations.ir.json

zeroship deploy --env=staging    # the staging app and control
zeroship migrate --env=prod --yes   # protected: --yes is required

zeroship secret set STRIPE_SECRET_KEY=sk_live_...   # value, never in the file
```

The flags survive as overrides where they always were: `zeroship deploy
./other.zship --app=<id> --control=<url>` still works, wins over the file, and
says so in its provenance lines.

## The schema is the source of truth

`schema/project-v1.json` is the one place any of this is written down. Both
readers are generated from it:

```bash
node schema/codegen.mjs   # -> sdks/vite-plugin/src/project-config/generated.ts
                          # -> crates/cli/src/project_config/generated.rs
```

The generated modules carry the known-key sets, the required-key sets, the
patterns and enums, the `x-cli-read` deny-list, and - on the TypeScript side
only - the defaults. `tests/project_config_gate.sh` fails when either generated
file drifts from the schema, and when the two readers disagree about the same
file.

## See also

- [`vite-plugin.md`](vite-plugin.md) - the plugin options that did not move
- [`env-vars.md`](env-vars.md) - `ZEROSHIP_CONFIG` and the rest of the environment surface
- [`zship.md`](zship.md) - the artifact `build.output` names
- [`../build-and-deploy-golden-path.md`](../build-and-deploy-golden-path.md) - build, deploy, migrate end to end
