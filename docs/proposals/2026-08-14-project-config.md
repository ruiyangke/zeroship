# `zeroship.jsonc`: the canonical creator project configuration

**Date:** 2026-08-14
**Status:** Proposed
**Scope:** the creator-facing project surface only. Explicitly NOT the platform
server config surface, which `docs/proposals/2026-08-11-config-name-alignment.md`
owns and which that proposal already places out of scope for the CLI
(`docs/proposals/2026-08-11-config-name-alignment.md:106-107`).

---

## 0. Reading conventions used in this document

Every claim about current behaviour carries a `file:line`. Claims are labelled:

- **VERIFIED** - I read the cited lines in this working tree at `e9fd101b2`.
- **INFERRED** - a conclusion drawn from verified facts, not itself read.
- **NOT CHECKED** - stated so the reader does not mistake silence for evidence.

An empty grep is reported as an empty grep, never as proof of absence. Section
2.7 lists what the enumeration method structurally cannot see.

---

## 1. Decision summary

`zeroship.jsonc` at the project root becomes the canonical creator project
configuration. Both the Vite plugin and the Rust CLI read it. The decision that
it should exist is taken; this document designs it.

The recommendation in one paragraph: put in the file exactly the facts that
**cross a tool boundary and do not vary per developer** - the deploy target
(`app`, `control`), the paths the build produces and the CLI consumes
(`migrations.dir`, `migrations.out`), and the build shape the packer needs
(`mode`, `serverEntry`, `dist`). Keep out of it everything a single tool reads
alone, everything that varies per machine, everything the **runtime** enforces,
and every secret. Solve the two-parser drift with a **schema-first single source
of truth** plus one cheap gate - not with a second `crates/config-contract`.
Write nothing back into the file except under an explicit, opt-in command.

### 1.1 What changed in revision 2, and why

A reader of the first version should know exactly what moved.

| Change | Reason |
| --- | --- |
| **Scope narrowed to an invariant** (1.2): the file serves the CLI and the build; **the runtime never reads it**. | The first version left the boundary implicit. An implicit boundary erodes; this one now has a stated invariant and a proposed gate (1.2). |
| **`defineApp` (`rpc`, `resources`, `net`) removed from the file entirely.** The first version had it as "moved, but land it separately". | It is not separable-but-included, it is **out of scope**. `rpc.defaults` and `resources` are facts about the app's *behaviour*, compiled into the manifest and enforced by the gateway at runtime. The file holds facts about how the *tooling* operates. See 7.4. |
| **The classification rule gained two more axes** (section 3): not just "who consumes the fact", but also "what varies per developer" and "what the runtime enforces". | The first version enumerated the split correctly and justified it case by case. The three-axis rule states the rule instead of the answers. Precedent: Cloudflare's Vite plugin keeps exactly the per-machine options and reads the rest from the file (section 3, 4.4). |
| **Added: locating the file** (4.4) and **the `config` escape hatch** (4.5). | The first version specified precedence for *values* but not for the *file*, and left no pressure valve for a computed value. Both gaps are filled by Cloudflare's shipped design, and the missing escape hatch is a plausible cause of `defineApp` being a `.ts` file in the first place. |
| **Section 11.2 ("no second parser") closed** rather than left open. | Two reasons it does not survive contact: Cloudflare had the identical option and declined it, and it breaks the fresh-clone case. See 11.2. |

Unchanged and still load-bearing: the four-derivation finding (2.1), schema-first
with **no Rust defaults** for cross-tool facts (7.2, 7.3), secrets as names only
with `devAuth[].password` deleted (8), `runtime_date` reserved with the mechanism
deferred (6), non-inheritable `app` / `control` with printed provenance (9), and
section 11.1 as a live objection.

### 1.2 Scope invariant: the CLI and the build, never the runtime

> **`zeroship.jsonc` is read by the `zeroship` CLI and by the build toolchain.
> It is never read by the runtime, never packed into a `.zship`, and never
> leaves the creator's machine. The `.zship` manifest is what a deployed app
> runs on.**

This is the line that decides every inclusion question below, and it is why
`defineApp` stays where it is (7.4). Two artifacts, two audiences:

| | `zeroship.jsonc` | `.zship` manifest |
| --- | --- | --- |
| Written by | the creator (plus one CLI writeback, 5.2) | the build (`sdks/vite-plugin/src/zship.ts:296-304`) |
| Read by | `zeroship` CLI, Vite plugin, `zeroship-dev-migrate`, `gen-types-all` | control plane, gateway, worker |
| Travels | never leaves the machine | uploaded on every deploy |
| Contains | how the tooling operates | how the app behaves |

**The invariant holds today by construction, and that is exactly why it will
erode quietly.** **VERIFIED**: the packer walks only `distDir`
(`sdks/vite-plugin/src/zship.ts:288-300`, walk entry at `:649`), and
`zeroship.jsonc` lives at the project root, one level above it. Nothing forbids a
future `copyPublicDir`-style step, a `dist/` that is the project root in some
static configuration, or a well-meaning "the gateway could read the resource tree
directly" change. The check must exist before the temptation does.

**Proposed enforcement, in `tests/project_config_gate.sh` alongside the
round-trip fixture (7.2):**

1. **Not packed.** Build a fixture app whose root holds a `zeroship.jsonc` with a
   recognisable sentinel, run the packer, and assert the sentinel appears
   **nowhere** in the emitted archive - not as an asset key in `manifest.assets`,
   not in any blob. Assert on the archive bytes, not on the asset map, so a
   future path that carries it as a blob without a manifest entry still fails.
   Pair it with a control that plants the same sentinel in `dist/` and **does**
   find it, so a gate that greps nothing cannot pass by accident.
2. **No runtime parser.** A source check that no crate in the runtime-side set
   (`crates/runtime`, `crates/worker`, `crates/gateway`, `crates/plugin-*`)
   mentions the config filename or links the config-parsing module. Today that
   set is clean - the only repo-wide hit for `jsonc` in those crates is a
   ```` ```jsonc ```` code-fence tag in a doc comment at
   `crates/zeroship-gateway/src/idempotency.rs:32` (**VERIFIED**) - so the gate starts
   green and stays meaningful.
3. **Parser lives in one crate.** The JSONC parsing dependency (5.2) is declared
   by `crates/cli` only. A `cargo tree` assertion that no runtime-side crate
   pulls it makes (2) hard to defeat by indirection.

Check (1) is the one that matters; (2) and (3) are cheap defence in depth. All
three run without a Postgres or a six-binary build, unlike
`tests/config_name_alignment_gate.sh:217`.

---

## 2. The current surface, enumerated

### 2.1 The motivating defect, re-verified and found larger than reported

The brief describes two spellings of one fact. There are **four**, and only one
of the four is configurable.

| # | Consumer | How it learns the gen-types output dir | Citation |
| --- | --- | --- | --- |
| 1 | Vite plugin (build) | `options.migrations?.genTypesOut ?? GEN_TYPES_OUT_DEFAULT` | `sdks/vite-plugin/src/build.ts:631` |
| 1b | Vite plugin (dev, packer) | same expression, four more sites | `sdks/vite-plugin/src/dev-server.ts:308`, `:318`, `sdks/vite-plugin/src/zship.ts:479` |
| 2 | `zeroship migrate` (Rust) | hardcoded `DEFAULT_IR_PATH` | `crates/zeroship-cli/src/migrate.rs:43` |
| 2b | `zeroship deploy` reminder (Rust) | same const | `crates/zeroship-cli/src/main.rs:423` |
| 3 | `gen-types-all.ts` (repo runner) | hardcoded `join(app.root, "migrations")` | `sdks/vite-plugin/scripts/gen-types-all.ts:136` |
| 4 | `zeroship-dev-migrate` (separate bin) | its own `--migrations` / `--out` flags with independently written defaults | `sdks/vite-plugin/src/cli/migrate-dev.ts:77-78` |

**VERIFIED.** The plugin's default is `GEN_TYPES_OUT_DEFAULT = "generated/zeroship"`
(`sdks/vite-plugin/src/gen-types/index.ts:60`) and the filename is
`MIGRATIONS_IR_FILE = "migrations.ir.json"` (`sdks/vite-plugin/src/gen-types/index.ts:52`),
joined at write time (`sdks/vite-plugin/src/gen-types/index.ts:419`, written at
`:436`). The Rust const is the literal concatenation of those two
(`crates/zeroship-cli/src/migrate.rs:43`). Set `genTypesOut: "src/gen"` and the build
writes `src/gen/migrations.ir.json`; `zeroship migrate` with no positional path
reads `generated/zeroship/migrations.ir.json`
(`crates/zeroship-cli/src/migrate.rs:57`) and fails at
`std::fs::read_to_string` (`crates/zeroship-cli/src/migrate.rs:72`). Nothing reconciles
them. The `.zship` archive is not a back-channel either: the packer explicitly
never carries migration documents (`sdks/vite-plugin/src/zship.ts:216-220`).

Consumer 3 is the most interesting, because somebody already hit this. The repo
runner **refuses to run** when it detects a `migrations: {` block or the token
`genTypesOut` in an app's `vite.config.*`:

> `gen-types-all: ... configures the gen-types inputs (migrations.dir /
> migrations.genTypesOut). This runner derives them from disk and will not
> guess - teach it to read the config, or drop the override.`
> (`sdks/vite-plugin/scripts/gen-types-all.ts:117-129`)

That error message is the proposal, written by an earlier author who chose to
fail loudly instead of building the file. **VERIFIED.** It also detects the
override by regex over the config *source text* - which is itself the shape a
machine-readable config file removes.

Consumer 4 is a second, independently shipped CLI that re-derives both defaults
by hand (`sdks/vite-plugin/src/cli/migrate-dev.ts:77-78`) and is wired into two
scaffolds as `"migrate": "zeroship-dev-migrate"`
(`sdks/create-zeroship-app/template/package.json`, `examples/db-todos/package.json`).

**Conclusion.** The defect is real, verified, and its true blast radius is four
independent derivations of two facts, one of which has already been converted
into a hard refusal rather than fixed.

### 2.2 `ZeroshipOptions` - the Vite plugin surface

`sdks/vite-plugin/src/index.ts:46-105`. **VERIFIED** by reading the interface and
each consumption site.

| Option | Declared | Default | Consumed at | Notes |
| --- | --- | --- | --- | --- |
| `rpcEndpoint` | `index.ts:48` | `"/_rpc"` (`constants.ts:81`) | `index.ts:108`, `index.ts:123` | **INERT.** `docs/reference/vite-plugin.md:64-69` classifies it under "Exposed but not currently effectful": stubs use the shipped `/__zeroship/v1/<wireId>` path regardless. |
| `serverEntry` | `index.ts:50` | auto-detected | `index.ts:126` -> `build.ts` | |
| `devServerPort` | `index.ts:52` | `3001` (`constants.ts:49`) | `dev-server.ts:527` | Two of three scaffolds compute it from an env var at config-eval time (see 2.6). |
| `mode` | `index.ts:64` | `"full"` | `index.ts:127`, `build.ts:427` | Decides whether `manifest.worker` is emitted. |
| `devAuth` | `index.ts:84` | `true` in dev | `index.ts:124` -> `dev-auth-config.ts` | Serialised into `ZEROSHIP_DEV_AUTH` JSON for the child. |
| `migrations.dir` | `index.ts:100` | `"migrations"` | `build.ts:626`, `dev-server.ts:317`, `:391`, `:609` | |
| `migrations.genTypesOut` | `index.ts:103` | `"generated/zeroship"` | `build.ts:631`, `dev-server.ts:308`, `:318`, `zship.ts:479` | The defect above. |

**Documentation rot, noted because it bears on how well-understood this surface
is.** The reference table at `docs/reference/vite-plugin.md:26-31` lists five of
these seven. `devAuth` appears nowhere in that file (**VERIFIED** by grep:
`grep -n "rpcEndpoint\|devAuth" docs/reference/vite-plugin.md` returns exactly
one line, `:69`, for `rpcEndpoint`). A creator reading the reference cannot
discover `devAuth` at all.

### 2.3 `defineApp` - a second creator config file, which is NOT in scope

This is the largest thing the brief's enumeration list does not mention. It is
enumerated here for completeness and then **excluded** by the scope invariant
(1.2); the reasoning is in 7.4. It is recorded because an enumeration that
omitted it would leave a reader believing `zeroship.jsonc` is the only
creator-facing declarative surface, which is false.

**VERIFIED.** `sdks/server/src/define-app.ts:41-46` exports `defineApp`. Its doc
comment (`define-app.ts:10-14`) reads:

> Lives at exactly **one** path: `<projectRoot>/src/server/config.ts`. The
> vite-plugin reads only this file. There is no `zeroship.config.ts` at the
> project root, no `zeroship.toml` for RPC defaults, no per-directory
> `$config.ts` - every app-level setting is declared here.

The shape is `AppDefinition { resources?, net?, rpc? }`
(`sdks/server/src/types.ts:230-243`). It is loaded from the fixed path
`src/server/config.ts` (`sdks/vite-plugin/src/manifest.ts:634-641`), and there is
a test asserting a root `zeroship.config.ts` is ignored
(`sdks/vite-plugin/test/manifest-resources.test.ts:408-430`).

**How it is parsed matters.** The plugin does not execute the module. It strips
import lines and `as` casts by regex, locates `defineApp(`, matches parens, and
`Function`-evals the slice (`sdks/vite-plugin/src/manifest.ts:610-700`). Its own
comment says so: "we extract the `defineApp({ resources: { ... } })` argument via
a coarse JS-evaluation approach ... Computed expressions (e.g.
`auth: env.PROD ? ... : ...`) fail with a clear message asking the user to flatten
the literal ... A full ts-morph based parse is future work."
(`sdks/vite-plugin/src/manifest.ts:612-620`).

**INFERRED.** A config surface that must be a flat literal, is parsed by regex
plus `eval`, and refuses computed expressions is a JSON document that has been
given a `.ts` extension. It costs the creator TypeScript ergonomics it cannot
deliver, and costs the platform a parser it has already documented as inadequate.

**That is a parsing defect, and it is not an argument for relocating this tree.**
See 7.4. The fix is a real parser (ts-morph, which the plugin's own comment
already names as the intended future work at
`sdks/vite-plugin/src/manifest.ts:619-620`), tracked as separate work.

### 2.4 The CLI surface

**VERIFIED** by direct read of the dispatch site (`crates/zeroship-cli/src/main.rs:52-63`)
and each subcommand. Nine subcommands: `serve`, `deploy`, `migrate`, `login`,
`logout`, `whoami`, `dev`, `secret`, `var`.

**Creator deploy-path commands:**

| Command | Flag | Default / fallback | Citation |
| --- | --- | --- | --- |
| `deploy` | positional `<path-to-.zship>` | required, `.expect()` panic | `crates/zeroship-cli/src/main.rs:353-355` |
| `deploy` | `--app=` | **required, no fallback of any kind, `.expect("--app=<name> is required")`** | `crates/zeroship-cli/src/main.rs:362` |
| `deploy` | `--control=` | `ZEROSHIP_CONTROL_URL`, then `"http://localhost:9090"` | `crates/zeroship-cli/src/main.rs:363-367` |
| `deploy` | `--token=` | `ZEROSHIP_TOKEN`, then saved credentials | `crates/zeroship-cli/src/main.rs:368-371`, `:912-938` |
| `deploy` | `--no-create` | presence-only; auto-create is ON by default | `crates/zeroship-cli/src/main.rs:698-700` |
| `deploy` | known-flag allow-list | `DEPLOY_KNOWN_FLAGS` | `crates/zeroship-cli/src/main.rs:867` |
| `migrate` | positional IR path | `DEFAULT_IR_PATH` | `crates/zeroship-cli/src/migrate.rs:43,57` |
| `migrate` | `--app=` | required | `crates/zeroship-cli/src/migrate.rs:58-63` |
| `migrate` | `--control=` | `ZEROSHIP_CONTROL_URL`, then `"http://localhost:9090"` | `crates/zeroship-cli/src/migrate.rs:64-68` |
| `migrate` | `--token=` | shared resolver | `crates/zeroship-cli/src/migrate.rs:69` |
| `migrate` | known-flag allow-list | `MIGRATE_KNOWN_FLAGS` | `crates/zeroship-cli/src/migrate.rs:52` |
| `secret` / `var` | `--app=` | required | `crates/zeroship-cli/src/secrets.rs:140` |
| `secret` / `var` | `--control=` | `ZEROSHIP_CONTROL_URL`, then `"http://localhost:9090"` | `crates/zeroship-cli/src/secrets.rs:141-145` |
| `secret` / `var` | `--token=` | shared resolver | `crates/zeroship-cli/src/secrets.rs:146-152` |
| `secret set` | `--expose` | presence-only | `crates/zeroship-cli/src/secrets.rs:168-175` |
| `login` | `--control=` | `ZEROSHIP_CONTROL_URL`, then `DEFAULT_CONTROL_URL` | `crates/zeroship-cli/src/auth.rs:85-94`, const `:11` |
| `login` | `--provider=` | `"platform"` | `crates/zeroship-cli/src/auth.rs:343-346,364-366` |

**`serve` (a local process, not a deploy-path command):** `--port` (default 3000,
`crates/zeroship-cli/src/main.rs:79`), `--workers` (0 = auto, `:80`), `--cpu-limit` (`:81`),
`--wall-timeout` (`:83`), `--heap-limit-mb` (default 512, `:89-95`), allow-list
`SERVE_KNOWN_FLAGS` at `:820-826`.

**`dev init` is an OPERATOR command, not a creator one.** It provisions the local
Docker Compose stack: `--secrets-dir` default `deploy/compose/secrets`, `--env-file`
default `deploy/compose/.env` (`crates/zeroship-cli/src/dev.rs:13-14,110-112`), and it
generates the platform secrets in `ENV_KEYS` (`crates/zeroship-cli/src/dev.rs`).
It is out of scope for a creator project file entirely.

**Three commands have no unknown-flag gate at all.** `secret`, `var`, and `login`
carry no `check_unknown_*` allow-list; an unrecognised `--foo` is silently
ignored (**VERIFIED by reading each file for the absence** - a grep for
`KNOWN_FLAGS` finds three arrays and would wrongly imply the other commands are
gated too). This matters below: adding a config-file layer to a command that
silently swallows typos makes a wrong deploy target *more* reachable, not less.

### 2.5 Environment variables a creator's toolchain reads

The Rust side registers every read through `declared_env!`
(`crates/zeroship-core/src/config/declared.rs:634-654`), which pushes a `DeclaredEnvRead`
into the `linkme` slice `DECLARED_ENV_READS`
(`crates/zeroship-core/src/config/declared.rs:428-430`). The CLI's consumer marker is
`ZeroshipCliConsumer` (`crates/zeroship-cli/src/main.rs:25-35`).

**VERIFIED** call sites in `crates/cli`:

| Name | Class | Site | Used by |
| --- | --- | --- | --- |
| `ZEROSHIP_CONTROL_URL` | cli | `main.rs:365`, `migrate.rs:66`, `secrets.rs:143`, `auth.rs:88-93` | deploy / migrate / secret / var / login |
| `ZEROSHIP_TOKEN` | cli | `main.rs:915` | all authenticated commands |
| `ZEROSHIP_CONFIG_HOME` | cli | `auth.rs:417-422` | credential store location |
| `XDG_CONFIG_HOME` | external | `auth.rs:423-428` | ditto |
| `HOME` | external | `auth.rs:429-432` | ditto |
| `ZEROSHIP_HEAP_LIMIT_MB` | cli | `main.rs:91` | `serve` |
| `DATABASE_URL` | external | `main.rs:170` | `serve` |
| `ZEROSHIP_STORAGE_URL` | cli | `main.rs:188-190` | `serve`, default `file://.zeroship/storage` |
| `ZEROSHIP_KV_URL` | cli | `main.rs:231-235` | `serve` |
| `ZEROSHIP_KV_PATH` | cli | `main.rs:244-250` | `serve`, default `.zeroship/kv.redb` |
| `ZEROSHIP_WORKFLOW_SQLITE_PATH` | cli | `main.rs:290-296` | `serve`, default `.zeroship/workflows.sqlite` |
| `ZEROSHIP_DIE_WITH_PARENT` | cli | `parent_death.rs:82-87` | every subcommand, armed at `main.rs:41` |
| whole-process snapshot | creator | `main.rs:286-288` | `serve` -> V8 `process.env` |

**There is no app-id environment variable.** `grep -rn "ZEROSHIP_APP" crates/ sdks/ docs/`
returns nothing (**VERIFIED**, empty result; see 2.7 for what that grep cannot
see). The app id is typed by hand on every single command, and its absence is a
panic, not an error message (`crates/zeroship-cli/src/main.rs:362`).

TS side, set by the Vite plugin for the spawned child
(`sdks/vite-plugin/src/constants.ts`): `ZEROSHIP_DEV` (`:8`),
`ZEROSHIP_VITE_ORIGIN` (`:9`), `ZEROSHIP_ENTRY` (`:10`),
`ZEROSHIP_RUNTIME_DESCRIPTOR` (`:11`), `ZEROSHIP_DEV_AUTH` (`:24`),
`ZEROSHIP_DEV_AUTH_SECRET` (`:25`), `ZEROSHIP_DIE_WITH_PARENT` (`:46`). These are
an internal parent-to-child transport, not a creator surface.

### 2.6 File and script conventions the scaffolds already rely on

**VERIFIED** by reading each file and by `git ls-files`.

- **`.env` at the project root is already a real creator config surface.**
  `sdks/vite-plugin/src/dev-database-url.ts:26-43` parses it;
  `sdks/vite-plugin/src/dev-database-url.ts:46-59` establishes the precedence
  **shell env > `.env` > dev default**; and the dev server splats the whole file
  into the child's environment: `const childEnv: NodeJS.ProcessEnv = { ...dotenvVars, ... }`
  (`sdks/vite-plugin/src/dev-server.ts:910-922`). Since the child snapshots its
  whole environment into V8's `process.env` (`crates/zeroship-cli/src/main.rs:286-288`),
  `.env` is functionally already this platform's `.dev.vars`.
- `.env` and `.env.local` are gitignored by the scaffold
  (`sdks/create-zeroship-app/template/_gitignore:5-6`), although no scaffold ships
  one today.
- `generated/zeroship/{env.db.ts,schema.runtime.json,migrations.ir.json}` **are
  committed** in both the template and `examples/db-todos` (**VERIFIED** via
  `git ls-files`).
- **No scaffold script passes `--app=` or `--control=`.** Scripts are `dev`,
  `build`, `typecheck`, plus `"migrate": "zeroship-dev-migrate"` in two of three
  (`sdks/create-zeroship-app/template/package.json`,
  `examples/db-todos/package.json`, `examples/starter/package.json`).
- `examples/starter/vite.config.ts:8` and `examples/db-todos/vite.config.ts:8`
  both compute `devServerPort` as `Number(process.env.<NAME>_API_PORT ?? 3001)`.
  This is a live use of the config being *executable code*, and a JSONC file
  cannot reproduce it. It constrains the design (see 4.2).
- The golden path tells the creator to type `--app=<id> --control=<url> --token=<PAT>`
  on both deploy and migrate (`docs/build-and-deploy-golden-path.md:64-71`), and
  the same doc already lists as an open item: "Smooth one-step deploy UX
  (provision-app-if-needed; a `zeroship deploy` that creates the app on first push
  instead of requiring a pre-created `--app`)"
  (`docs/build-and-deploy-golden-path.md:176-182`).

### 2.7 What this enumeration cannot see

Stated explicitly, because this codebase has produced wrong "zero hits" answers
before.

1. **`format!`-constructed names.** A grep for a literal name cannot see a
   variable assembled at runtime. The repo's own source census admits this exact
   gap: `crates/zeroship-config-contract/tests/declared_env.rs:153-158` documents a
   "KNOWN BLIND SPOT ... a `macro_rules!` body is tokens rather than expressions,
   so this source census cannot see it." I checked `crates/cli` specifically for
   `format!` near env reads and found only URL and error-string construction -
   **VERIFIED for `crates/cli` only**, not for the plugin crates it links.
2. **Link-time versus source-time.** `DECLARED_ENV_READS` is populated by whatever
   the binary *links*. If `zeroship-runtime`, `plugin-db`, `kv-v8`,
   `plugin-storage` or `plugin-workflow` read environment variables internally,
   those reads are in the shipped `zeroship` binary but are invisible to a grep of
   `crates/zeroship-cli/src/`. **NOT CHECKED.** The authoritative enumeration is to run a
   program that dumps the linked slice, which I did not do.
3. **Cross-crate delegation.** `serve` hands resolved values to the runtime; the
   runtime's own fallbacks (for example the 128 MB heap default at
   `crates/zeroship-runtime/src/core/serve.rs:55`, unreachable from the CLI because the CLI
   always passes `Some`) are only visible by reading the callee.
4. **Absence of a gate is invisible to a grep for gates.** The finding that
   `secret`, `var` and `login` have no unknown-flag allow-list came from reading
   each file for the *absence* of `check_unknown_*`. A `KNOWN_FLAGS` grep returns
   three hits and silently implies the rest are covered.
5. **Regex-detected config.** `sdks/vite-plugin/scripts/gen-types-all.ts:122`
   detects an override by matching `/\bgenTypesOut\b/` against config source text.
   That will match a comment or a string and miss a spread (`...opts`). Anything
   downstream of it inherits both errors.
6. **The `zeroship.config.ts` "clean absence" is a filename absence, not a
   concept absence.** The grep found no such file, but the concept lives at
   `src/server/config.ts` under a different name (2.3). A name-shaped search would
   have reported "no project config exists" and been wrong.
7. **Scope, which bit this document twice.** The brief names three scaffolds
   (`sdks/create-zeroship-app/template/`, `examples/starter/`,
   `examples/db-todos/`). A first pass enumerated only those and concluded the
   whole deletion set was unused. Widening to all 30 `examples/*/vite.config.ts`
   found `mode` live in `examples/ssg-docs/vite.config.ts:74`, `devAuth` live in
   five examples, and `devAuth[].password` live in six call sites (8.3, 10.4).
   Both empty greps were correct; neither was the question being asked. Any
   reviewer extending this enumeration should widen before trusting a zero.
8. **Word matches inside comments.** A grep for `migrations` hits
   `examples/db-todos/vite.config.ts:11`, which is prose in a comment, not an
   option. Every "live call site" count in section 10.4 was re-checked by reading
   the matched line; the raw grep count was wrong by one.

---

## 3. Classification: the three-axis test

The test is **not** "would it be convenient in a file". A fact belongs in
`zeroship.jsonc` only if it passes **all three**:

- **Axis A - crosses a tool boundary.** Is it produced by one tool and consumed
  by another? If no, a file buys nothing and costs a parser.
- **Axis B - the same for every developer on the project.** If it varies per
  machine, per checkout, or per concurrently-running example, a committed file is
  the wrong home no matter how many tools read it.
- **Axis C - the runtime does not enforce it** (the 1.2 invariant). If the fact
  is compiled into the `.zship` manifest and acted on by the gateway or worker,
  it is a fact about the *app*, not about the *tooling*, and it belongs on the
  path that travels with the app.

Revision 1 used axis A alone and reached the right split by arguing each case.
Three axes state the rule instead of the answers, which is what lets a future
option be classified without reopening this document.

**Precedent.** Cloudflare's Vite plugin lands on the same split. Its surviving
plugin options are `configPath`, `persistState` (`.wrangler/state`),
`inspectorPort` (`9229`), `tunnel`, `viteEnvironment`, `auxiliaryWorkers`,
`remoteBindings`, and `config` - every one of them a per-developer-machine
concern or a pointer at the file (**VERIFIED** against
`https://developers.cloudflare.com/workers/vite-plugin/reference/api/`, fetched
2026-08-14). Everything about the deployed Worker is read from `wrangler.jsonc`.
That is axis B doing the work: the plugin option bag is what varies per machine,
the file is what does not.

Applied here, the two surviving `ZeroshipOptions` fields are `devServerPort` and
`devAuth` - both per-machine, both exactly the shape of `inspectorPort` and
`persistState`. **INFERRED**, but it is a striking convergence: two independent
designs, one boundary.

### 3.1 Belongs in `zeroship.jsonc` (passes A, B and C)

| Fact | Produced by | Consumed by | Why it must move |
| --- | --- | --- | --- |
| `app` | the creator (or `zeroship deploy` auto-create, `crates/zeroship-cli/src/main.rs:698-700`) | `deploy`, `migrate`, `secret`, `var` - four commands, every invocation | Retyped on every command today; absent it is a **panic** (`crates/zeroship-cli/src/main.rs:362`). Nothing else in the project records which app this directory deploys to. |
| `control` | the creator / operator | same four commands plus `login` (`crates/zeroship-cli/src/auth.rs:85-94`) | Today the only per-project memory is a shell env var. A wrong default silently targets `http://localhost:9090` - which the CLI's own comment calls out as unrecoverable for `migrate` (`crates/zeroship-cli/src/migrate.rs:46-51`). |
| `migrations.dir` | the creator | Vite build (`build.ts:626`), dev server (`dev-server.ts:317`), repo runner (`gen-types-all.ts:136`), `zeroship-dev-migrate` (`migrate-dev.ts:77`) | Four derivations, one of which refuses to run rather than guess. |
| `migrations.out` (renamed from `genTypesOut`) | Vite build (`gen-types/index.ts:419,436`) | `zeroship migrate` (`migrate.rs:43,57`), `zeroship deploy` reminder (`main.rs:423`), `zeroship-dev-migrate` (`migrate-dev.ts:78`) | **The canonical case.** Produced by the build, consumed by a Rust binary that cannot read `vite.config.ts`. |
| `build.dist` / `build.output` | Vite packer (`zship.ts:250-252,267,271`) | the creator, who types `zeroship deploy ./dist/app.zship` (`docs/build-and-deploy-golden-path.md:68`) | Same producer/consumer split, one step less painful because the path appears in the build's own output. Moving it lets `zeroship deploy` take zero positional arguments. |
| `mode` (`"full"` / `"static"`) | the creator | Vite build today (`index.ts:127`); **and** anything that wants to know whether a `.zship` should carry a worker | Borderline. See 3.5 for the argument that it should move anyway. |
| `serverEntry` | the creator | Vite build (`index.ts:126`) | Borderline; moves for coherence with `mode`, not on its own merits. |

Note what is **absent** from this table and was present in revision 1:
`resources` / `rpc` / `net`. They fail axis C - see 3.2 and 7.4.

### 3.2 Excluded by the scope invariant: runtime-enforced facts

| Fact | Where it stays | Why it cannot move |
| --- | --- | --- |
| `resources` (the policy tree) | `src/server/config.ts` | Compiled into `Manifest.resources` (`crates/zeroship-bundle/src/manifest.rs:51-59`) and turned into per-resource `EffectivePolicy` records by the **gateway** at app-load time. It is enforced on every end-user request, on a machine the creator does not own. |
| `rpc.defaults` | `src/server/config.ts` | Same path: app-wide defaults inherited by procedures, resolved into the manifest and enforced at dispatch. |
| `net` | `src/server/config.ts` | "Inert outbound TCP request hints ... the control plane diffs them against operator-authored grants for review" (`sdks/server/src/types.ts:232-236`). The consumer is the control plane, reached via the deploy artifact. |

These are the sharpest test of the invariant, because they are genuinely
declarative, genuinely creator-authored, and genuinely badly parsed today - every
surface property of a `zeroship.jsonc` field except the one that counts. Full
argument in 7.4.

### 3.3 Stays a flag or environment variable only (axis B: varies per developer)

| Fact | Why it must not move |
| --- | --- |
| `--token` / `ZEROSHIP_TOKEN` | Secret. Already resolved flag > env > `~/.config/zeroship/token.json` at mode `0600` (`crates/zeroship-cli/src/auth.rs:412-436`, `:439-448`). A tracked file must never be able to supply it. |
| `devServerPort` | Must vary per machine and per concurrently-running example. Two of three scaffolds compute it from an env var at config-eval time (`examples/starter/vite.config.ts:8`, `examples/db-todos/vite.config.ts:8`). A committed value would make two examples collide. Keep it a plugin option and add `ZEROSHIP_DEV_PORT` as the per-machine override. |
| `DATABASE_URL` (dev) | Per-machine, already has a working home in `.env` with documented precedence (`sdks/vite-plugin/src/dev-database-url.ts:46-59`). Moving it to a tracked file is a regression. |
| `serve` runtime limits (`--cpu-limit`, `--wall-timeout`, `--heap-limit-mb`, `--workers`, `--port`) | These configure a **local process**, not the app. The deployed equivalents are per-app `AppRuntimeLimits` held by the control plane (`docs/reference/runtime-limits.md`). Putting local process tuning in the app's config file invites the belief that it applies in production. It does not. |
| `dev init` flags | Operator command for the Compose stack (`crates/zeroship-cli/src/dev.rs:13-15`). Not a creator surface. |
| `ZEROSHIP_CONFIG_HOME` / `XDG_CONFIG_HOME` / `HOME` | Machine identity, not project identity. |
| `ZEROSHIP_STORAGE_URL`, `ZEROSHIP_KV_URL`, `ZEROSHIP_KV_PATH`, `ZEROSHIP_WORKFLOW_SQLITE_PATH` | Local `serve` backend selection. Per-machine. |

### 3.4 Genuinely build-only - keep in `vite.config.ts`

Anything Vite alone reads and no other tool ever needs: `plugins`, `resolve`,
`server.watch` (`examples/db-todos/vite.config.ts:20`), `build.rollupOptions`.
Not in scope.

### 3.5 The one judgement call: `mode`

`mode` is read only by the Vite build today (`sdks/vite-plugin/src/index.ts:127`,
`sdks/vite-plugin/src/build.ts:427`). By the strict producer/consumer test it
should stay a plugin option.

It moves anyway, for one reason: `mode: "static"` is the difference between a
`.zship` that carries a worker and one that does not, and a creator debugging
"why does my deploy 404 on `/api`" has no way to see that fact except by reading
`vite.config.ts`. Putting it in the declarative file makes the deploy shape
inspectable by every tool including the eventual `zeroship inspect`. **This is
the weakest inclusion in the proposal and I flag it as such** - if the reviewer
prefers the strict test, drop `mode` and `serverEntry` and the design is unharmed.

### 3.6 Deletions: `rpcEndpoint`

`rpcEndpoint` is inert. `docs/reference/vite-plugin.md:64-69` classifies it under
"Exposed but not currently effectful" and states the stubs use
`/__zeroship/v1/<wireId>` regardless. Pre-launch, an option that does nothing is
deleted, not migrated. It does not appear in `zeroship.jsonc`.

---

## 4. The file

### 4.1 Shape

```jsonc
// zeroship.jsonc - committed. No secrets. See `zeroship secret` for those.
{
  "$schema": "https://zeroship.ai/schema/project-v1.json",

  // Required. The deploy target.
  "name": "my-app",
  "app": "app_2Zk9xQ...",          // written by `zeroship deploy` on first push
  "control": "https://control.zeroship.ai",

  // Required. See section 6 - this may be deferred to its own decision.
  "runtime_date": "2026-08-14",

  "build": {
    "mode": "full",                 // "full" | "static"
    "serverEntry": "src/index.ts",  // omit for auto-detection
    "dist": "dist",
    "output": "dist/app.zship"
  },

  "migrations": {
    "dir": "migrations",
    "out": "generated/zeroship"
  },

  // Secret NAMES only. Values live in `.env` (dev) / `zeroship secret` (deployed).
  "secrets": ["STRIPE_SECRET_KEY", "OPENAI_API_KEY"],

  // NOT here: `rpc` / `resources` / `net`. Those are runtime-enforced app
  // behaviour and stay in src/server/config.ts. See 3.2 and 7.4.

  "environments": {
    "staging": {
      "app": "app_7Bn2mR...",
      "control": "https://control.staging.zeroship.ai"
    }
  }
}
```

### 4.2 Required keys

Wrangler requires `name`, `main`, `compatibility_date` (**VERIFIED** against
Cloudflare's current configuration doc, fetched 2026-08-14). The zeroship
analogue is `name`, `app`, `control`, and - if section 6 is accepted -
`runtime_date`. `main` has no analogue: the server entry is discovered, and
discovery works (`sdks/vite-plugin/src/index.ts:50` documents `serverEntry` as an
override of auto-detection, not a requirement).

Deliberately **not** required: `app`. A brand-new project has no app id, and
`zeroship deploy` already auto-creates by default
(`crates/zeroship-cli/src/main.rs:698-700`). The first deploy writes it back (5.2).

### 4.3 Format: JSONC, and why not plain JSON or TOML

The brief asks the plain-JSON option to be argued against Cloudflare's move.
Here is the honest accounting.

- **JSONC.** Comments survive. Cloudflare moved TOML to JSONC deliberately and
  now recommends `wrangler.jsonc` for new projects, with some newer features
  JSON-config-only (**VERIFIED** against the current doc). The cost is
  comment-preserving round-trip on writeback, which is a real engineering cost
  (5.2).
- **Plain JSON.** Dodges round-trip entirely - `serde_json` in, `serde_json` out.
  The cost is that the file cannot explain itself, and this file will contain
  `"mode": "static"` and `"runtime_date"`, both of which need a sentence. The
  scaffold would have to put the explanations in `README.md`, where they rot -
  and this repo has a documented pattern of exactly that rot
  (`docs/reference/vite-plugin.md:26-31` omitting `devAuth`).
- **TOML.** Rejected: the platform's own server-config ADR already chose TOML for
  the operator overlay (`docs/decisions/2026-05-28-server-config-unification.md`),
  and using the same format for a creator-facing file that has different rules
  (no secrets ever, writeback allowed) would invite the two to be confused.

**Recommendation: JSONC**, and pay the round-trip cost, because the writeback
surface is deliberately kept to a single scalar (5.2) which makes the cost small
and boundable. If the reviewer will not accept comment-preserving writeback, the
fallback is JSONC with **writeback removed entirely** (`zeroship deploy` prints
the line to paste) - not plain JSON. Losing comments is a worse trade than losing
automatic writeback.

### 4.4 Locating the file

Revision 1 specified precedence for **values** (5.1) and forgot precedence for
the **file**. Cloudflare's plugin resolves the entry Worker's config in this
order (**VERIFIED**, same source as 3.0):

1. the `configPath` plugin option
2. the `CLOUDFLARE_VITE_WRANGLER_CONFIG_PATH` environment variable
3. auto-discovery of `wrangler.jsonc`, then `wrangler.json`, then `wrangler.toml`
   in the app root

Adopt the shape exactly, with one simplification:

1. **`configPath`** - a `zeroship()` plugin option, and a `--config=` flag on
   every CLI subcommand that reads the file.
2. **`ZEROSHIP_CONFIG=<path>`** - an environment variable. This is the monorepo
   and CI lever: one checkout, several apps, and a wrapper that does not want to
   `cd`.
3. **Auto-discovery of `zeroship.jsonc` in the app root** - and nothing else. No
   `.json` fallback and no `.toml` fallback, because 4.3 chose one format and
   pre-launch has no legacy files to accept. Cloudflare's three-way search is a
   back-compat artifact; `AGENTS.md` forbids us the equivalent.

**Auto-discovery is what makes the common case `zeroship()` with no arguments**,
which is what the template ships today
(`sdks/create-zeroship-app/template/vite.config.ts:6`) and must keep shipping.

**No upward directory walk.** The file is found in the app root or it is not
found. A walk means a `pnpm build` in a subdirectory can silently pick up a
sibling app's `app` and `control` - the same cross-targeting hazard section 9
exists to close, arriving through the file-location door instead. **INFERRED**;
I know of no incident, but the cost of forbidding it now is zero.

**When no file is found**, the CLI errors naming the three ways to supply one,
and the plugin proceeds on its own defaults (which is exactly today's behaviour,
and is what keeps `zeroship()` working in a scratch directory). That asymmetry is
deliberate and is the same one 7.3 sets up: the plugin has defaults, the CLI has
none.

### 4.5 The `config` escape hatch

Cloudflare ships a `config` plugin option: "Customize or override Worker
configuration programmatically. Accepts a partial configuration object or a
function that receives the current config. Applied after any config file loads."
(**VERIFIED**, same source.)

**Adopt it, and understand what it is for.** A purely declarative file has one
failure mode: the first creator who needs a computed value has no move except to
abandon the file. This tree already shows what that looks like - eight examples
compute a port from a bespoke `process.env.*_API_PORT`
(`examples/starter/vite.config.ts:8` and seven siblings, 10.1), and
`src/server/config.ts` is a `.ts` file whose parser then has to refuse computed
expressions anyway (`sdks/vite-plugin/src/manifest.ts:612-620`). **INFERRED, and
I think it is the most useful inference in this document: the absence of an
escape hatch is a plausible cause of `defineApp` being TypeScript in the first
place.** A declarative file without a pressure valve does not stay declarative;
it grows a `.ts` sibling.

Design:

```ts
zeroship({
  config: (c) => ({ ...c, build: { ...c.build, mode: process.env.SSG ? "static" : "full" } }),
})
```

- Accepts a **partial object** (shallow-merged over the loaded file) or a
  **function** `(resolved) => partial`, applied **after** the file loads and
  after environment selection, so it sees exactly what the tooling resolved.
- **It may not override any field the CLI also reads.** That is `name`, `app`, `control`,
  `migrations.dir`, `migrations.out`, `build.output`, and `runtime_date`.
  Attempting to set one is an **error naming the field**, not a silent drop.

That restriction is the whole design, and it is not a nicety. A `config`
function runs inside Vite. The Rust CLI cannot execute it and never will - it is
the same wall that produced the original defect (`crates/zeroship-cli/src/migrate.rs:9-14`
is explicit that the CLI does not evaluate `.ts`). Letting the hatch touch a
CLI-read field would reintroduce the exact drift this proposal removes, wearing
a feature's clothing. Fields the **plugin alone** reads (`build.mode`,
`build.serverEntry`, `build.dist`) are overridable, because there is only one
reader and it is the one running the function.

**Enforcement is free**: the set of CLI-read fields is already a machine-readable
list, because the JSON Schema generates the Rust struct (7.2). Mark those
properties `"x-cli-read": true` in the schema, generate the deny-list into the TS
side from the same source, and the restriction cannot drift from the fact it
protects.

---

## 5. Precedence and writability

### 5.1 Precedence

**`flag > env > zeroship.jsonc (selected environment) > zeroship.jsonc (root) > compiled default`.**

This extends the server-side order established by
`docs/decisions/2026-05-28-server-config-unification.md:30-46` and restated at
`docs/proposals/2026-08-11-config-name-alignment.md:36-38`, inserting the
environment overlay between file-root and default. It also matches what the CLI
already does for `--control` (`crates/zeroship-cli/src/main.rs:363-367`) and `--token`
(`crates/zeroship-cli/src/main.rs:920-938`), so no existing behaviour inverts.

**One addition, and it is the important one.** Because three commands silently
swallow unknown flags (2.4), every command that resolves a value from the file
must **print the resolved value and its source** on stderr before acting -
exactly as the dev-database resolver already does
(`sdks/vite-plugin/src/dev-database-url.ts:61-72`, which prints
"using DATABASE_URL from `.env`" versus "from shell environment"). A config file
that silently supplies a control URL is strictly more dangerous than a flag that
must be typed. The provenance line is what makes it safe, and it is cheap.

Concretely, `zeroship migrate` should print
`control: https://control.staging.zeroship.ai (from zeroship.jsonc environments.staging)`
before it POSTs. The CLI's own comment at `crates/zeroship-cli/src/migrate.rs:46-51`
explains why: applying a migration set to the wrong database "is not something an
error message afterwards can undo."

### 5.2 Writability

Wrangler writes provisioned resource IDs back into the config on deploy
(**VERIFIED**). The zeroship analogue is exactly one field: `app`, written by
`zeroship deploy` when it auto-creates (`crates/zeroship-cli/src/main.rs:388-392` already
reports `created app {name} ({id})` to stderr and then throws the id away).

**Design:**

- **Exactly one writable field: `app`** (and `environments.<name>.app`). Nothing
  else. Not `control` - that would let a `--control=` typo become permanent.
- **Written only on the auto-create path**, never on a normal deploy. If `app` is
  already present the file is not opened for writing at all.
- **A surgical scalar edit, not a re-serialise.** Locate the `"app"` member's
  value span in the parsed CST and splice. Comments, key order, and whitespace
  outside that span are byte-identical. This is a much smaller problem than
  general comment-preserving round-trip and is what makes JSONC affordable.
- **If `app` is absent entirely**, the CLI appends the member after `name` with a
  generated comment, or - simpler and my preference for v1 - **refuses to write
  and prints the exact line to paste**. Insertion into arbitrary JSONC is where
  round-trip libraries get ugly; splice-only is provably safe.

Implementation note (**INFERRED**, not measured): the Rust side needs a
JSONC-with-spans parser. `jsonc-parser` and `json_spanned_value` exist; neither is
in this workspace today (**VERIFIED** - `schemars` is absent workspace-wide, and
I found no JSONC crate). Adding one dependency for the CLI is proportionate;
adding one for the runtime would not be, and is not needed.

---

## 6. `compatibility_date`: a bigger decision, scoped separately

**VERIFIED: zeroship has no per-app runtime version pin.** A repo-wide grep for
`compatibility_date|compat_date|runtime_version|api_version` finds only Stripe's
`api_version` in the billing tests, and `schema_version` in `plugin-db`, which is
a per-app DDL counter (`crates/zeroship-data-v8/src/register_model/bootstrap.rs`),
not a runtime behaviour pin. `Manifest.version` (`crates/zeroship-bundle/src/manifest.rs:33-39`)
is a **wire-format** version - "Schema version. Reject unknown values" - not a
behaviour selector. Nothing in the tree lets a deployed app say "give me the V8
runtime semantics of date D".

**The brief is right that this is larger than the file, and I recommend scoping
it out.** Here is the position, stated so a future decision has a starting point:

- **A pin belongs, and pre-launch is when it is cheap.** The moment two apps
  exist that disagree about a runtime behaviour change, the platform either
  freezes the behaviour forever or breaks one of them. `AGENTS.md` is explicit
  that "Native primitives are forever"; a date pin is the escape hatch that makes
  that survivable.
- **But it is not free, and its cost is not in this file.** A pin is only
  meaningful if the runtime can actually *branch* on it - which means every
  behaviour change from that day forward carries a dated gate, the worker caches
  isolates per (app, deploy, runtime-date), and there is a policy for how long
  old dates are honoured. That is a runtime and worker design problem, and none
  of it is built.
- **Recommendation: reserve the key, do not implement the mechanism.**
  `zeroship.jsonc` declares `runtime_date` as a **required** field from day one,
  the scaffold writes today's date, the CLI validates the format and carries it
  into the manifest, and the runtime **ignores it** until the first behaviour
  change needs it. Requiring it now costs one line in the scaffold. Adding it
  later costs a migration of every creator project - which pre-launch means
  nothing, but the *habit* of writing it is what has value, and the field being
  absent is what makes the first gated change expensive.
- If the reviewer disagrees, the fallback is to omit it and open a separate ADR.
  I would not smuggle the mechanism in under a config-file proposal, and this
  section deliberately does not.

---

## 7. Two parsers, one file

### 7.1 The risk is not the field names - it is the defaults

The server-side problem `crates/config-contract` solves is **N spellings of one
setting across flag, env, TOML, Compose and docs**. That is not this problem.
Here, both readers read the *same* file with the *same* key names; a
misremembered key name fails loudly on both sides.

The failure mode that actually survives is **divergent defaults**. If
`zeroship.jsonc` omits `migrations.out`, the TS side falls back to
`GEN_TYPES_OUT_DEFAULT` (`sdks/vite-plugin/src/gen-types/index.ts:60`) and the
Rust side falls back to `DEFAULT_IR_PATH` (`crates/zeroship-cli/src/migrate.rs:43`) - and
we have reproduced the exact bug the file was built to remove, one layer up. A
second, quieter mode: a key the TS side reads and the Rust side silently ignores,
so the config *looks* honoured.

### 7.2 Options

**Option A - port the config-contract shape.** Two independent derivations that
must agree in both directions, as `tests/config_name_alignment_gate.sh` checks 2
and 3 do.

*Cost, measured.* The server-side apparatus is roughly 6,000 LOC in
`crates/zeroship-config-contract/`, 1,654 LOC in `crates/zeroship-config-macros/`, 5,264 LOC in
`crates/zeroship-core/src/config/`, and 2,552 LOC of shell gates - approximately 18,000
LOC total, requiring a `cargo build` of six server binaries in CI
(`tests/config_name_alignment_gate.sh:217`). It exists because a name mismatch
silently boots a production service on a wrong default, an incident class the
gate's own comments date to 2026-08-13.

*Verdict: rejected as disproportionate.* Fifteen fields, one file, two readers,
no Compose surface, no multi-binary fan-out. **REJECT.**

**Option B - schema-first, generate both sides, one gate. RECOMMENDED.**

- `schema/project-v1.json` is a hand-written JSON Schema and is **the single
  source of truth**, including every `default`.
- `sdks/vite-plugin/src/project-config.ts` (TS types + defaults) and
  `crates/zeroship-cli/src/project_config.rs` (serde structs + defaults) are both
  **generated from it** and committed.
- One gate, `tests/project_config_gate.sh`, does three things: (1) regenerate
  both and `diff` against the committed files (the `env-vars-doc --check` shape
  already used at `tests/config_name_alignment_gate.sh` step 4); (2) assert every
  schema property with a `default` appears in both generated files with that
  literal; (3) a **round-trip fixture** - a single `zeroship.jsonc` in
  `tests/fixtures/`, parsed by both binaries, each dumping resolved JSON, and the
  two dumps must be byte-equal.
- Cost: **INFERRED, not measured** - roughly 300 lines of schema plus two small
  codegen scripts plus a ~100-line shell gate. No `cargo build` of six binaries;
  it needs `zeroship` and `node`.

Check (3) is the load-bearing one, and it is the cheap analogue of "both
derivations agree in both directions": it catches divergent defaults, silently
ignored keys, and type coercion differences in a single comparison, without a
second parser being written to catch the first.

**Option C - accept the risk, no gate.** Rely on review. *Verdict: reject.* This
repo has the receipts: `sdks/vite-plugin/scripts/gen-types-all.ts:117-129` is a
hard refusal added because review did not catch the drift, and
`docs/reference/vite-plugin.md:26-31` omits two live options today.

### 7.3 A cheaper structural answer, which strengthens Option B

**Give the Rust side no defaults at all for cross-tool facts.**

The scaffold always writes `migrations.dir` and `migrations.out` explicitly. If
`zeroship.jsonc` is present and the key is absent, `zeroship migrate` **errors**
naming the key rather than falling back. The TS side keeps its defaults, because
a plugin must work with `zeroship()` and no file at all (this is what
`sdks/create-zeroship-app/template/vite.config.ts` does today).

The result: exactly one place in the world holds the default for a cross-tool
fact, and it is the JSON Schema. Check (2) of the gate then has only one
generated file to verify rather than two, and the Rust side becomes incapable of
reproducing the original bug by construction.

**This is a design constraint, not just an enforcement trick,** and it is the
part of the recommendation I am most confident about.

### 7.4 Why `defineApp` does NOT move (reversed from revision 1)

Revision 1 put the `resources` / `rpc` / `net` tree in the file and called the
decision "separable". That was wrong, and the correction is worth spelling out
because the wrong answer is the attractive one.

**The case for moving it, which is genuinely strong.** Its parser is a regex plus
`Function`-eval that the plugin itself documents as inadequate
(`sdks/vite-plugin/src/manifest.ts:612-620`). It already forbids computed
expressions, so it is a JSON document wearing a `.ts` extension (2.3). It is
creator-authored and declarative. `docs/decisions/` contains no ADR defending
`src/server/config.ts` - only a doc comment asserting it
(`sdks/server/src/define-app.ts:10-14`). Every one of those is true and none of
them is refuted below.

**Why it is not enough.** The parser is bad; that is a **parsing** problem. Its
fix is a real parser - ts-morph, which the plugin's own comment already names as
the intended work (`sdks/vite-plugin/src/manifest.ts:619-620`) - and that fix is
available without moving a single field. Revision 1 used a defect in *how* the
tree is read as an argument about *where* the tree should live. Those are
independent, and conflating them would have bought a better parser at the price
of a boundary.

**What the boundary actually is** (axis C, 1.2): `resources` and `rpc.defaults`
are compiled into `Manifest.resources` (`crates/zeroship-bundle/src/manifest.rs:51-59`)
and turned into per-resource `EffectivePolicy` records by the gateway at
app-load time. They are enforced on every end-user request on infrastructure the
creator does not own. `net` is diffed by the control plane against
operator-authored grants (`sdks/server/src/types.ts:232-236`). These are facts
about **the app's behaviour**; they travel in the `.zship` and are read by the
runtime. `migrations.out` and `control` are facts about **how the tooling
operates**; they never leave the machine. One file per audience.

**The concrete cost of getting it wrong.** Put the policy tree in
`zeroship.jsonc` and one of two things follows. Either the file is packed into
the `.zship` so the gateway can read it - which breaks the 1.2 invariant
outright, gives the runtime a second policy source alongside
`Manifest.resources`, and creates a live question about which wins. Or it is
compiled into the manifest as it is today - in which case the move bought
nothing except a longer file and a field the `config` escape hatch (4.5) must
now be taught to refuse. Neither is worth having.

**Recorded as separate work, not as a follow-on to this proposal:** replace the
regex-plus-eval extractor at `sdks/vite-plugin/src/manifest.ts:610-700` with a
real TypeScript parse. That work is now strictly smaller than revision 1 made it,
because nothing has to move first.

---

## 8. Secrets

### 8.1 The rule

**`zeroship.jsonc` declares secret NAMES. It never holds a value.** This matches
Wrangler exactly (**VERIFIED**: the config doc says do not store sensitive
information in `vars`, use the `secrets` property to declare required secret
names for validation and type generation, and put actual values in `.dev.vars` or
`.env`).

### 8.2 Where the values already go - both channels exist

This is the part that makes the rule cheap: **zeroship has already built both
halves of Wrangler's split.**

- **Deployed values:** `zeroship secret set KEY=value --app=<uuid> [--expose]`
  (`crates/zeroship-cli/src/secrets.rs:3-9`). Values live in the control plane. The
  `--expose` list is the per-app opt-in deciding which secrets also reach
  `process.env`, because `process.env` "is readable by any npm dependency without
  the creator writing a line of code" (`crates/zeroship-cli/src/secrets.rs:15-19`). Key
  rule: 1-64 bytes, `^[A-Z][A-Z0-9_]*$` (`crates/zeroship-cli/src/secrets.rs:31`,
  `:404-413`).
- **Local dev values:** `<root>/.env`, parsed at
  `sdks/vite-plugin/src/dev-database-url.ts:26-43`, splatted whole into the dev
  runtime child at `sdks/vite-plugin/src/dev-server.ts:910-922`, and gitignored by
  the scaffold (`sdks/create-zeroship-app/template/_gitignore:5-6`).
- **Platform credentials:** never in the project at all -
  `~/.config/zeroship/token.json` at mode `0600`
  (`crates/zeroship-cli/src/auth.rs:412-436`, `:439-448`).

So `.env` is already `.dev.vars`, and `zeroship secret` is already
`wrangler secret put`. The `"secrets": [...]` array in `zeroship.jsonc` adds one
thing and one thing only: a **declared expectation** the tooling can check. The
deliverable is a pre-deploy warning - "`zeroship.jsonc` declares
`STRIPE_SECRET_KEY`; `zeroship secret list --app=...` does not show it" - and a
dev-boot warning when `.env` lacks a declared name.

### 8.3 Where `devAuth.password` goes

`DevAuthUser.password` is at `sdks/vite-plugin/src/index.ts:43`. Two things are
true and the second is the one that matters.

**First, it is not actually a secret.** Its own doc comment says so: "Not a
secret - it only makes the dev credential check (and its failure path) real"
(`sdks/vite-plugin/src/index.ts:38-43`). The default is the well-known literal
`DEFAULT_DEV_PASSWORD` (`sdks/bootstrap/src/dev-auth.ts:127`), the dev login form
**prefills** it (`sdks/bootstrap/src/dev-auth.ts:522-528`), and the provider is
structurally absent from any production `.zship`
(`sdks/vite-plugin/src/index.ts:80-83`). Nothing here leaks.

**Second, the shape is the problem.** A field named `password` in a file named
`zeroship.jsonc` that a creator commits will, eventually, receive a real
password - because the creator will want their dev login to match their staging
login, and the field accepts a string. The defence is not documentation; the
defence is that the field must not exist in that file.

**Design:**

1. **`devAuth` does not appear in `zeroship.jsonc` at all.** It is a dev-only
   concern read by exactly one tool (the Vite plugin) and therefore fails the
   producer/consumer test on its own merits, before the secrets argument.
2. It **stays a `vite.config.ts` plugin option**, where it is today. `vite.config.ts`
   is also committed, so this is not a security improvement by itself - which is
   why:
3. **`password` is deleted from `DevAuthUser`.** The remaining fields (`id`,
   `email`, `name`, `avatar`, `scopes`) carry no secret shape. The dev password
   becomes the well-known constant, with no per-user override.
4. **A per-user dev password is genuinely in use and must be replaced, not just
   removed.** **VERIFIED** - three examples set it:
   `examples/auth-notes-db/vite.config.ts:21,27` (`"alice"` / `"bob"`),
   `examples/auth-uploads-kv/vite.config.ts:30,36` (same), and
   `examples/auth-probe/vite.config.ts:36,44` (`"probe-pw"`). The purpose is
   visible in the values: distinct passwords per identity so a multi-user login
   test can tell the users apart, and so a wrong-password failure path is
   exercisable.

   That need is real and survives the deletion. The replacement is a **derived**
   password rather than an authored one: the dev-auth provider makes each user's
   password its own `id` (or a short deterministic function of it). Distinct per
   user, exercisable failure path, zero authored strings, and the field that
   invites a real secret is gone. The three examples lose two lines each and
   their tests assert against `user.id` instead of a literal.

   If a literal is still wanted for a specific case, it comes from `.env` as
   `ZEROSHIP_DEV_PASSWORD_<userid>` - untracked, per-machine, and consistent with
   how `ZEROSHIP_DEV_AUTH_SECRET` already works (generated fresh per dev server,
   never persisted - `sdks/vite-plugin/src/constants.ts:18-25`).

   **I had this wrong on the first pass.** I grepped only the three scaffolds
   named in the brief, found nothing, and was about to record "no evidence anyone
   uses it". Widening to `examples/*/vite.config.ts` found six live call sites.
   The empty grep was real; its scope was not the question.

### 8.4 Enforcement

The server side's plaintext-secret rule is **documentary, not mechanical**:
`deploy/ops/zeroship.toml:51-61` states "this file is tracked, and a plaintext
secret in a tracked file is unrecoverable once committed" and there is simply no
secret table, by discipline. **VERIFIED** - no gate enforces it.

For the creator file, do better, because it is cheap:

- The JSON Schema sets `"additionalProperties": false` on every object. A
  `"password"` or `"token"` key is a **parse error**, on both sides, before the
  value is read.
- The schema forbids the key names `password`, `token`, `secret`, `key`,
  `apiKey`, and `credentials` anywhere in the tree, with an error message naming
  `zeroship secret set` and `.env`.
- `"secrets"` is `{"type": "array", "items": {"type": "string", "pattern": "^[A-Z][A-Z0-9_]{0,63}$"}}` -
  the same rule the CLI already enforces (`crates/zeroship-cli/src/secrets.rs:404-413`). An
  array of strings cannot carry a value; there is no place to put one.

That is a structural guarantee rather than a convention, and it costs three
schema clauses.

---

## 9. Environments and the staging-versus-production hazard

Wrangler makes bindings **non-inheritable** so staging cannot silently inherit
production's resources (**VERIFIED**). The brief is right that the direct
analogue does not exist: `env.db` / `env.kv` / `env.storage` are ambient, injected
per-app by the platform, never declared by the creator (`AGENTS.md`, SDK layers
section). There is nothing to inherit.

**The real hazard is the deploy target,** and it is worse than Wrangler's,
because `control` has a *default* and `app` does not. Concretely
(**VERIFIED** from `crates/zeroship-cli/src/migrate.rs:64-68`): if the file's `control` is
production and the creator meant staging, `zeroship migrate` applies a migration
set to the production database. The CLI's own comment on
`MIGRATE_KNOWN_FLAGS` says a mistake here "is not something an error message
afterwards can undo" (`crates/zeroship-cli/src/migrate.rs:46-51`).

**Design, borrowing Wrangler's non-inheritance principle and pointing it at the
right field:**

1. **`app` and `control` are non-inheritable.** Every entry under `environments`
   must state **both**, explicitly, or the file fails to parse. An environment
   that names a `control` and inherits the root `app` is precisely the silent
   cross-targeting Wrangler's rule exists to prevent. Everything else
   (`build`, `migrations`, `secrets`, `resources`) inherits normally.
2. **No implicit environment.** `--env=<name>` must be passed; there is no
   "default environment" and no `ZEROSHIP_ENV` fallback. An env var that silently
   switches which database gets migrated is the same failure with extra steps.
3. **Provenance on stderr, always** (5.1). Print the resolved `app` and `control`
   with their source before any mutating call.
4. **A destructive-command confirmation, gated on a marker.** An environment may
   carry `"protected": true`. `zeroship migrate` against a protected environment
   requires `--yes` or an interactive confirmation. This is the only mechanism
   here that stops a *correct* config from being run at the wrong moment, and it
   costs one boolean.

**INFERRED**: (4) has no precedent in this tree - I found no confirmation prompt
in any CLI subcommand. It is a new behaviour and the reviewer may reasonably
scope it out; (1) through (3) are the load-bearing ones.

---

## 10. Migration path

Pre-launch. `AGENTS.md`: rename, delete the old name, one PR. No aliases, no
detect-and-warn.

### 10.1 Deleted from `ZeroshipOptions` (`sdks/vite-plugin/src/index.ts:46-105`)

| Option | Disposition |
| --- | --- |
| `rpcEndpoint` | **Deleted outright.** Inert (`docs/reference/vite-plugin.md:64-69`). Not replaced. Delete `DEFAULT_RPC_ENDPOINT` (`sdks/vite-plugin/src/constants.ts:81`) and the parameter threaded through `index.ts:108,123` and `build.ts:544`. |
| `migrations.dir` | **Deleted from the option.** Moves to `zeroship.jsonc` `migrations.dir`. |
| `migrations.genTypesOut` | **Deleted from the option.** Moves to `zeroship.jsonc` `migrations.out` - renamed, because "genTypes" names the producer and the directory is read by four consumers. |
| `mode` | **Deleted from the option.** Moves to `build.mode`. |
| `serverEntry` | **Deleted from the option.** Moves to `build.serverEntry`. |
| `devServerPort` | **Kept** as a plugin option (3.3). Add `ZEROSHIP_DEV_PORT` as the per-machine override so examples can stop inventing per-example port variables. **VERIFIED**: 17 examples pass `devServerPort`, and 8 of those compute it from a bespoke `process.env.<NAME>_API_PORT` (`CSR_TODO_API_PORT`, `DB_CHAT_API_PORT`, `DB_E2E_API_PORT`, `DB_TODOS_API_PORT`, `HR_SYSTEM_API_PORT`, `STARTER_API_PORT`, `STORAGE_GALLERY_API_PORT`, and one more). That is eight undeclared names doing one job. |
| `devAuth` | **Kept** as a plugin option, with `DevAuthUser.password` **deleted** (`sdks/vite-plugin/src/index.ts:43`, and `password` handling at `sdks/bootstrap/src/dev-auth.ts:97,105-106,290-312,347-353,533-543`). |

`ZeroshipOptions` after this change is two fields: `devServerPort` and `devAuth`.
**That is a result worth stating plainly** - it suggests the plugin option bag was
carrying facts that were never build-only, which is the same conclusion the
enumeration reached from the other direction.

### 10.2 Deleted elsewhere

- **`sdks/vite-plugin/scripts/gen-types-all.ts:117-129` - `assertNoConfigOverride`
  is deleted.** It exists solely because the runner could not read the config. It
  now reads `zeroship.jsonc`, and the hardcoded `join(app.root, "migrations")` at
  `:136` goes with it. This is the clearest single proof the file was needed.
- **`crates/zeroship-cli/src/migrate.rs:43` - `DEFAULT_IR_PATH` is deleted** (7.3). The path
  comes from the file or the command errors naming the key. The positional
  override at `crates/zeroship-cli/src/migrate.rs:57,99-103` stays as an escape hatch;
  `crates/zeroship-cli/src/main.rs:423`'s reminder reads the resolved path.
- **`sdks/vite-plugin/src/cli/migrate-dev.ts:77-78` - the `--migrations` / `--out`
  defaults are deleted**; the flags stay as overrides, defaults come from the file.
**Explicitly NOT deleted**, reversing revision 1: `src/server/config.ts`,
`sdks/server/src/define-app.ts`, the extractor at
`sdks/vite-plugin/src/manifest.ts:610-700`, and the test block at
`sdks/vite-plugin/test/manifest-resources.test.ts:378-434` all stay exactly as
they are. See 7.4. The extractor's replacement with a real TypeScript parse is
separate work and is not a dependency of anything here.

### 10.3 What the scaffolds ship instead

- **`sdks/create-zeroship-app/template/`**: add `zeroship.jsonc` with `name`
  (from the directory name the scaffolder already prompts for), `control`,
  `runtime_date`, and explicit `build` / `migrations` blocks (7.3 requires them
  explicit). `app` is **absent** - the first `zeroship deploy` auto-creates and
  writes it (5.2). `vite.config.ts` stays `zeroship()` with no options, which it
  already is.
- **`examples/starter/`**: add `zeroship.jsonc`. `vite.config.ts` keeps
  `zeroship({ devServerPort })` but reads `ZEROSHIP_DEV_PORT` instead of
  `STARTER_API_PORT` (`examples/starter/vite.config.ts:8`).
- **`examples/db-todos/`**: add `zeroship.jsonc` with the `migrations` block.
  `vite.config.ts` keeps `devServerPort` and the `server.watch.ignored` entry
  (`examples/db-todos/vite.config.ts:20`), which is pure Vite and stays.
- **`package.json` scripts gain the deploy step they lack today** (2.6):
  `"deploy": "zeroship deploy"` with no flags, because the file supplies them.
  This is the visible payoff, and it is what closes the golden path's own open
  item at `docs/build-and-deploy-golden-path.md:176-182`.
- **Docs:** `docs/reference/vite-plugin.md:24-31` becomes a two-row table; a new
  `docs/reference/project-config.md` documents the file; the golden path's typed
  commands (`docs/build-and-deploy-golden-path.md:64-71`) lose their flags.
  `docs/feature-map.md` rows for `zeroship migrate` and the vite plugin need the
  same edit - flagged because this repo has a documented habit of updating prose
  and leaving the table.

### 10.4 What breaks for an existing `vite.config.ts`

**Nothing in production, because there is none.** `AGENTS.md`: no published users,
no creator apps in the wild. The concrete breakage set is every `vite.config.ts`
in this repository that passes a deleted option.

**VERIFIED** by inspecting all 30 example configs plus the template
(`for f in examples/*/; do grep -o '<option names>' "$f/vite.config.ts"; done`):

| Option | Live call sites in this tree |
| --- | --- |
| `rpcEndpoint` | **zero** |
| `serverEntry` | **zero** |
| `migrations.dir` / `migrations.genTypesOut` | **zero** (`examples/db-todos/vite.config.ts:11` matches the word only in a comment - checked) |
| `mode` | **one**: `examples/ssg-docs/vite.config.ts:74` -> `zeroship({ mode: "static" })` |
| `devServerPort` (**kept**) | 17 examples |
| `devAuth` (**kept**, minus `password`) | 5: `auth-notes-db`, `auth-probe`, `auth-uploads-kv`, `env-probe`, `error-probe` |
| `devAuth[].password` (**deleted**) | 3 examples, 6 call sites (8.3) |

So the breakage is: **one config** (`examples/ssg-docs`) gains a
`zeroship.jsonc` with `"build": { "mode": "static" }` and loses its plugin
argument, and **three configs** drop `password` lines per 8.3. Everything else in
the deletion set is dead surface.

**This is a strong argument for doing it now and a weak argument for the file's
urgency** - see 11. Note also that this table is the second time the enumeration
had to be widened: a first pass scoped to the three scaffolds named in the brief
reported the deletion set as entirely unused, and both `mode` and `password` were
outside that scope.

---

## 11. The case against this proposal

### 11.1 The strongest objection: flags plus better defaults would do

The concrete pain is: a creator types `--app=<id> --control=<url>` on four
commands. The file's headline benefit is not typing them. But there is a cheaper
fix that is one afternoon of work:

- Add `ZEROSHIP_APP`, resolved exactly like `ZEROSHIP_CONTROL_URL` already is
  (`crates/zeroship-cli/src/main.rs:363-367`). Ten lines, four call sites.
- Put both in `.env`, which already exists, is already gitignored, and is already
  parsed and forwarded (`sdks/vite-plugin/src/dev-database-url.ts:26-43`,
  `sdks/vite-plugin/src/dev-server.ts:910-922`).
- Fix `crates/zeroship-cli/src/main.rs:362`'s `.expect()` panic into an error naming the
  env var.

That solves the typing problem completely, adds **no** new file, **no** second
parser, **no** JSONC round-trip, **no** JSON Schema, **no** codegen, and **no**
gate. And it has a genuine security advantage: `.env` is gitignored, so a staging
control URL and an app id never enter git at all.

**And section 10.4 undercuts the urgency.** The deletion set is almost entirely
dead surface: `rpcEndpoint`, `serverEntry` and `migrations.*` have **zero** live
call sites, and `mode` has exactly one (`examples/ssg-docs/vite.config.ts:74`).
The `genTypesOut` bug is real but is triggered only by overriding a default that
nothing in this tree overrides. The measured blast radius today is **one
hypothetical creator who changes a directory name**.

**The Cloudflare precedent does not refute this objection, and it would be
dishonest to deploy it as if it did.** Cloudflare has bindings, environments,
Durable Objects, routes, cron triggers, D1, R2, KV, queues, and a decade of
accumulated surface; a config file is unarguable at that size. Zeroship has
about a dozen fields. "The big platform has one" is an argument about where this
platform is going, not about where it is - and section 11.1 is precisely the
claim that we should wait until it gets there.

### 11.2 CONSIDERED AND REJECTED: one parser, with the build as sole producer

Revision 1 raised this as an open question and called it "a genuinely better
architecture on the drift axis". It is now closed. The argument and the two
reasons it fails are both recorded, because a rejected alternative that is not
written down gets re-proposed.

**The alternative.** Do not give the Rust CLI a parser at all. `zeroship migrate`
already treats the IR body as opaque - `crates/zeroship-cli/src/migrate.rs:9-14` is
explicit that the CLI "does not build, parse or rewrite it". Extend that shape:
the build emits `generated/zeroship/project.resolved.json`, flat and
machine-written, every path already absolute and every default already applied.
The Rust side reads three string fields, and has no schema, no defaults, no JSONC
parser, and no possibility of divergence. The creator-facing config could then
stay TypeScript, where it can compute a value - a capability JSONC removes and
section 4.5 has to reintroduce as an escape hatch.

**Why it is rejected:**

1. **It does not survive a fresh clone.** `zeroship migrate --app=... --control=...`
   must work before any build has run - so must `zeroship secret list`,
   `zeroship var set`, and `zeroship deploy` against a `.zship` built elsewhere
   (which `deploy/scripts/deploy-app.sh:195-197` explicitly supports via
   `--zship` without `--dir`). Under this design, `app` and `control` would be
   produced by the build, so a clean checkout could not name its own deploy
   target until it had built. That is a worse first-five-minutes than the one we
   have, and `app` is the single field with the strongest case for being in a
   file at all (3.1).
2. **The one platform that faced this exact fork declined it.** Cloudflare has
   the same CLI-plus-Vite-plugin split and the same two-language problem. They
   did not make the plugin the producer and the CLI a consumer of a generated
   artifact; **both tools read the same `wrangler.jsonc`**, and the dynamic cases
   are served by the `config` option applied after the file loads (**VERIFIED**,
   `https://developers.cloudflare.com/workers/vite-plugin/reference/api/`, fetched
   2026-08-14). That is not proof they were right, but it is a strong signal that
   the generated-artifact shape does not hold up at scale, and adopting `config`
   (4.5) is how this proposal buys the same flexibility without the fork.

**What survives from it.** The instinct is right that two parsers is a cost, and
7.3 is the concession: the Rust side gets **no defaults**, so it is a reader of
explicit values rather than a second authority. That captures most of the benefit
- disagreement becomes impossible rather than policed - at none of the cost.

### 11.3 The third objection: `runtime_date` is being reserved on faith

Section 6 asks for a required field the runtime ignores. If the pin mechanism is
never built - and nothing in `docs/proposals/` schedules it (**VERIFIED**: no
proposal mentions `compatibility_date`) - then every zeroship project carries a
meaningless date forever, and the first creator to ask what it does gets told
"nothing yet". Cloudflare's `compatibility_date` earns its keep because the
runtime branches on it. Ours would not.

### 11.4 Why I still recommend it

Three facts move me past 11.1, and only three:

1. **`sdks/vite-plugin/scripts/gen-types-all.ts:117-129` already exists.** Someone
   hit this and shipped a hard refusal because there was no file to read. That is
   not a hypothetical; it is a feature that is worse than it should be, today, in
   this tree.
2. **Four independent derivations of two facts** (2.1), not two. Env vars fix the
   `--app` typing problem but do nothing for `migrations.out`, because the value
   is a build artifact location, not a user preference.
3. **The build already produces a fact the CLI must consume** (`migrations.out`,
   3.1). No amount of environment-variable ergonomics addresses that, because the
   value is a build artifact location, not a user preference. This is the one
   argument in the list that 11.1 cannot answer at all.

**But 11.1 is a legitimate alternative and should be rejected explicitly rather
than by omission.** If the reviewer wants the smallest change that removes the
most pain, `ZEROSHIP_APP` in `.env` is it, and it is defensible. This proposal is
the larger bet that the surface keeps growing - and the evidence that it does is
that it has already grown to four spellings without anyone deciding it should.

**One argument revision 1 made here has been withdrawn.** It claimed
"`src/server/config.ts` already is this file", and used that to argue the choice
was not whether to have a declarative config but which one to have. Under the
scope invariant that is wrong: `src/server/config.ts` is a *runtime-policy*
surface and `zeroship.jsonc` is a *tooling* surface, so the existence of one is
not evidence for the other. Its bad parser is separate work (7.4). Removing this
argument makes the case for the file weaker and 11.1 correspondingly stronger,
which is why it is called out rather than quietly dropped.

---

## 12. Recommendation, in order

1. **Land the file** with `name`, `app`, `control`, `build.*`, `migrations.*`,
   `secrets[]`, `environments`. JSONC (4.3). Schema-first (Option B, 7.2) with
   the no-Rust-defaults constraint for cross-tool facts (7.3).
2. **Adopt the scope invariant (1.2) with its gate**: not packed into the
   `.zship`, no runtime-side parser, JSONC dependency confined to `crates/cli`.
   The not-packed check needs its one-variable control so a gate that greps
   nothing cannot pass.
3. **Locate the file** by `configPath` option / `--config=` flag, then
   `ZEROSHIP_CONFIG`, then auto-discovery of `zeroship.jsonc` in the app root.
   No format fallbacks, no upward walk (4.4).
4. **Ship the `config` escape hatch** (4.5), with the CLI-read fields
   (`app`, `control`, `migrations.*`, `build.output`, `runtime_date`) generated
   into its deny-list from the same schema.
5. **Delete** `rpcEndpoint`, `migrations.dir`, `migrations.genTypesOut`, `mode`,
   `serverEntry` from `ZeroshipOptions`; delete `DevAuthUser.password` at all six
   call sites; delete `DEFAULT_IR_PATH` and `assertNoConfigOverride` (10.1, 10.2).
6. **Writeback: `app` only, auto-create path only, splice-only** (5.2). If the
   round-trip proves awkward, ship v1 with no writeback and print the line.
7. **Reserve `runtime_date`; do not build the mechanism** (6). Open a separate ADR.
8. **Environments: `app` and `control` non-inheritable; no implicit environment;
   provenance printed on stderr before every mutating call** (9).

**Explicitly NOT in this proposal**, reversing revision 1: `defineApp` /
`src/server/config.ts` does not move (7.4). Replacing its regex-plus-eval
extractor with a real TypeScript parse
(`sdks/vite-plugin/src/manifest.ts:610-700`) is separate work with no dependency
on any of the above.
