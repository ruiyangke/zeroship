# Configuration name alignment

**Date:** 2026-08-11
**Status:** Proposed

## Decision summary

Give every in-scope platform startup setting one canonical dotted `snake_case`
identity. Generate its CLI flag, environment variable, TOML path, secret
handling, redaction, and machine-readable contract from that identity. Do not
store any of those spellings independently.

For platform servers and one-shots, the canonical projections are:

- CLI: kebab-case, local to the consuming binary.
- Environment: `ZEROSHIP_` plus the complete canonical identity in
  `SCREAMING_SNAKE_CASE`.
- TOML: the canonical dotted path, for operational and secret settings alike.

Delete the deployment alias hops. In particular, control's database setting
will no longer travel through `CONTROL_DATABASE_URL`,
`ZEROSHIP_DATABASE_URL`, `[secrets].database_url`, and finally
`DATABASE_URL`/`--db`. Its one identity will be `control.database_url`, with
the environment spelling `ZEROSHIP_CONTROL_DATABASE_URL`. The same rule gives
gateway and worker distinct environment names because their database roles are
distinct settings (`deploy/compose/docker-compose.yml:373-394` and
`deploy/compose/docker-compose.yml:430-470`).

Operational values may be supplied by flag, environment, or the optional TOML
overlay. Secret values may be supplied by environment or by the TOML overlay,
each as either a literal or a `urn:`/`arn:` reference, or by file path. A
secret's CLI input remains a generated `-file` path flag and never a value
flag, so a secret never reaches a process argument list. The prohibition on
plaintext secrets moves from the TOML FORMAT to TRACKED FILES, enforced by the
repository gate in Section 4.7. Source precedence remains
`CLI > env > file > compiled default`, as required by the accepted ADR
(`docs/decisions/2026-05-28-server-config-unification.md:30-46`).

Enforcement combines generated declarations with compiled contract, TOML,
rendered-Compose, runtime unknown-environment, and compiler-resolved raw-access
gates. The declared consumer set is therefore complete rather than
aspirational.

## Scope and binding context

The accepted design chose clap derive, serde, and toml; made the file overlay
optional; fixed precedence; and required `--check-config` on the four original
web binaries (`docs/decisions/2026-05-28-server-config-unification.md:30-50`).
It rejected config-rs, figment, confique, and a required primary config file
(`docs/decisions/2026-05-28-server-config-unification.md:60-69`). This proposal
does not reopen those choices.

The initial server conformance set is `zeroship-control`, `zeroship-gate`,
`zeroship-worker`, `zeroship-auth`, and `zeroship-migrated`.

AMENDED 2026-08-12. That set is the CONVERSION order, not the scope. There are
SIX server binaries and SEVEN targets classified `platform` in workspace
metadata: the five above, plus `zeroship-workflow-scheduler`
(`crates/workflow-scheduler/Cargo.toml:7-10`), plus the
`zeroship-platform-migrate` one-shot
(`crates/zeroship-migrate-adapter/Cargo.toml:30-33`). Text elsewhere in this
document, and the scope line of
`docs/proposals/2026-08-12-secure-by-default-config.md`, said "five"; the
metadata classification has said six-plus-one since Step 1, and it is the
authority the anti-vacuity check reads. See the scheduler note below for its
actual state.

Migrated is a
deployed long-running service and receives the shared overlay mount
(`deploy/compose/docker-compose.yml:285-330`), but its parser currently ends
without `--config` or `--check-config` and initializes tracing directly
(`crates/migrated/src/main.rs:20-119`). The same declaration and environment
access rules apply workspace-wide to other production zeroship processes. For
example, the standalone scheduler has its own clap/env spellings
(`crates/workflow-scheduler/src/main.rs:13-52`), while the deployed platform
migration one-shot still has a manual argument parser and a raw
`DATABASE_URL` fallback
(`deploy/compose/docker-compose.yml:110-176` and
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:73-113`).
Those processes must be registered before the final gate turns green, even
though only long-running servers need the full `--check-config` report.

AMENDED 2026-08-12, the scheduler's actual state, so a later step does not
assume it is done. Step 2 moved its clap definition out of `main.rs` into
`crates/workflow-scheduler/src/config.rs` so the compiled checker can link it,
and deliberately did NOT convert its fields. As of this amendment:

- Its fields are hand-spelled `#[arg(long, env = "...")]` with the
  `WORKFLOW_SCHEDULER_*` family, not the `ZEROSHIP_` projection, so it is the
  one platform server that would fail the Section 4.1 rule that all in-scope
  platform env names begin `ZEROSHIP_`.
- It declares no `ConfigSpec`, no read sites, and none of the six shared
  identities. It has no `--config`, no `--check-config`, and no overlay.
- Its `main` is a placeholder that logs and `exit(1)`: the dispatch, ack and
  registration loop still runs in the control cron, and the standalone process
  must not run as a production scheduler until that loop moves
  (`crates/workflow-scheduler/src/lib.rs:1-11`). Its metadata classification is
  `platform` with a reason that says so.

That last point is why it is in scope but not converted: it is a real workspace
bin target the anti-vacuity check must account for, and it will be a real
service, but converting configuration for a process that refuses to start buys
nothing until the extraction lands. Step 3 must convert it or the step is not
closed; Step 7 cannot pass while its env family is unprefixed.

The creator CLI and sandbox remain outside this server-config
overlay, matching the accepted ADR (`docs/decisions/2026-05-28-server-config-unification.md:56-58`).
Test-only, dev-only, and ambient operating-system variables are classified in
the contract but do not acquire TOML settings.

## Evidence and problem statement

The catalogue records 178 distinct Rust environment names, of which 167 are
production-reachable and 11 test-only
(`docs/reference/env-vars.md:1-19`). It also records that the scan cannot see
names assembled at runtime and intentionally excludes dependency-owned reads
(`docs/reference/env-vars.md:12-19`). The catalogue's reproduction commands
are at `docs/reference/env-vars.md:322-342`.

The supplied measurement found 136 distinct clap-declared names and 42
raw-only names. The command recorded for that measurement applies the
catalogue extraction separately and takes the set difference; it is included
here so the asserted counts have a reproducible origin, not to establish a
second inventory:

```bash
tmp="$(mktemp -d)"
grep -rEo 'env *= *"[A-Z][A-Z_0-9]*"' crates/ libs/ \
  | grep -oE '"[A-Z][A-Z_0-9]*"' | tr -d '"' | sort -u > "$tmp/clap"
grep -rEo 'env::var(_os)?\("[A-Z][A-Z_0-9]*"\)' crates/ libs/ \
  | grep -oE '"[A-Z][A-Z_0-9]*"' | tr -d '"' | sort -u > "$tmp/raw"
cat "$tmp/clap" "$tmp/raw" | sort -u | wc -l   # 178
wc -l < "$tmp/clap"                              # 136
comm -23 "$tmp/raw" "$tmp/clap" | wc -l        # 42
rm -rf "$tmp"
```

Raw reads are not confined to bootstrap code. Representative current reads
occur in the worker handler (`crates/worker/src/handler.rs:1700-1701` and
`crates/worker/src/handler.rs:3530-3532`), control HTTP utility code
(`crates/control/src/http_util.rs:157-159`), and runtime networking
(`crates/runtime/src/node/net/connect.rs:394-399` and
`crates/runtime/src/node/net/connect.rs:438`). Metering further hides several
names behind a string-taking helper
(`crates/metering/src/outbox.rs:88-103`), illustrating why a literal-only grep
cannot be the source of truth.

The current config layer is typed and already rejects unknown TOML keys, but
the schema is independently hand-written. `FileConfig` and its sections
are declared in `crates/core/src/config/file.rs:40-60`; section fields are
separate Rust identifiers and secret references are plain `Option<String>`
values (`crates/core/src/config/file.rs:111-171`). An env reference accepts any
non-empty text after `urn:zeroship:env:` and resolves it through a raw env read
(`crates/core/src/config/secrets.rs:280-359`). `--check-config` deliberately
validates only reference syntax, not whether the target has a consumer
(`crates/core/src/config/secrets.rs:389-403`). The live-file unit test merely
parses `deploy/ops/zeroship.toml` and checks `observability.log_filter`
(`crates/core/src/config/file.rs:342-353`).

That separation has produced concrete drift:

- Control's DSN starts as the Compose substitution
  `CONTROL_DATABASE_URL`, is assigned to container env
  `ZEROSHIP_DATABASE_URL`, is named by `[secrets].database_url`, and reaches a
  clap field declared as `--db`/`DATABASE_URL`
  (`deploy/compose/docker-compose.yml:200-209`,
  `deploy/ops/zeroship.toml:53-74`, and
  `crates/control/src/main.rs:50-66`). Control resolves those independent slots
  manually (`crates/control/src/main.rs:694-720`).
- The same middle `ZEROSHIP_DATABASE_URL` name carries gateway and worker DSNs
  even though Compose intentionally gives them different role-specific values
  (`deploy/compose/docker-compose.yml:373-394` and
  `deploy/compose/docker-compose.yml:466-472`).
- The example overlay points `postmark_webhook_password` at
  `POSTMARK_WEBHOOK_PASSWORD`
  (`deploy/ops/zeroship.example.toml:115-116`), while auth declares only
  `AUTH_POSTMARK_WEBHOOK_PASSWORD`
  (`crates/auth/src/config.rs:498-514`). The environment catalogue records this
  as a dead setting that looks live (`docs/reference/env-vars.md:297-308`).
- Compose itself documents environment entries on control that no longer have
  a control-process consumer (`deploy/compose/docker-compose.yml:222-247`). It
  also supplies `PROVISION_DATABASE_URL` to control
  (`deploy/compose/docker-compose.yml:204-209`) even though migrated is the
  parser that declares the provisioning DSN
  (`crates/migrated/src/main.rs:40-47`).

The current secret merge API makes drift easy: `obtain_secret` accepts a
free-form label, a clap-merged string, and an unrelated TOML string, then
applies precedence without a shared identity
(`crates/core/src/config/secrets.rs:451-481`). Current parser fields also still
accept secret values as flags, including control's DSN and keys
(`crates/control/src/main.rs:50-110`), gateway's DSN and keys
(`crates/gateway/src/main.rs:41-79` and
`crates/gateway/src/main.rs:133-159`), and worker's DSN, keys, and credentialed
KV URL (`crates/worker/src/main.rs:42-115`). This proposal replaces those value
flags; it does not add another precedence arm.

## 1. Canonical naming rule

### Canonical identity

A canonical name is an ASCII, lowercase, dotted `snake_case` path:

```text
segment       = [a-z][a-z0-9]*("_"[a-z0-9]+)*
canonical     = segment("."segment)*
```

No segment is reserved. The TOML projection of every canonical name is the
canonical name itself, for operational and secret settings alike, so there is
no second namespace to collide with and no carve-out to remember. The registry
still checks the final projected paths for uniqueness.

AMENDED 2026-08-12. This previously reserved a top-level `secrets` segment,
because secrets projected to `secrets.C` rather than `C`. That prefix split one
component's settings across two tables on a TRANSPORT property, in a design
whose stated rule is that a name describes one semantic setting and not one
transport. The reserved word existed only to stop an operational `secrets.foo`
colliding with the secret projection of `foo`; removing the prefix removes the
collision and the reservation together. See Section 2 for where secrets now sit
and what still separates them.

The name describes one semantic setting, not one transport. It includes a
service or domain prefix when the value is not platform-global. Separate values
that happen to have the same type get separate names. Thus control, gateway,
worker, auth, and migrated database URLs are separate settings because the
deployment gives them distinct roles or consumers
(`deploy/ops/zeroship.toml:53-60`,
`deploy/compose/docker-compose.yml:313-323`,
`deploy/compose/docker-compose.yml:373-394`,
`deploy/compose/docker-compose.yml:430-470`, and
`deploy/compose/docker-compose.yml:579-592`). A value that must be identical
across consumers has one platform-global canonical name. The current deployment
documents that requirement for the shared control key
(`docs/reference/env-vars.md:99-108`).

Each binary declares its local scope (`control`, `gateway`, `worker`, `auth`,
or `migrated`). For CLI ergonomics only, the flag projection removes one
leading canonical segment when it exactly equals that binary scope. No other
abbreviation is allowed. Therefore `control.database_url` becomes local flag
stem `database-url`, but `auth.platform_issuer` remains
`auth-platform-issuer` when consumed by control or migrated.

### Transform table

Let `local(C, B)` remove the leading `B.` from canonical name `C`, if present.
Let `kebab` replace dots and underscores with hyphens. Let `env` uppercase the
complete canonical name, replace dots with underscores, and prepend
`ZEROSHIP_`. Because the CLI and environment projections collapse both dots
and underscores, the registry rejects any pair of canonical names whose
projected environment names collide globally or whose projected flags collide
within a consumer binary.

| Supply class | Canonical `C` | CLI in binary `B` | Environment | TOML overlay |
| --- | --- | --- | --- | --- |
| Operational | `C` | `--kebab(local(C, B))` | `env(C)` | dotted path `C` |
| Secret | `C` | `--kebab(local(C, B))-file` path only | `env(C)` literal or file reference | dotted path `C`, literal or file reference |
| Bootstrap | `C` | generated flag when enabled | `env(C)` when enabled | disabled because it selects or controls loading |
| Command control | `C` | generated flag | disabled | disabled |
| Platform one-shot | `C` | generated flag under its operational/secret rule | `env(C)` when enabled | disabled |
| Dev/debug | `C` | disabled | `env(C)` in declared dev builds | disabled |
| External/ambient | external fixed name | disabled | protocol or OS spelling | disabled |

TOML dotted paths are normally written as tables. For example,
`worker.max_pinned_isolates_per_app` is
`[worker] max_pinned_isolates_per_app = 4`, while secret
`control.database_url` is
`[control] database_url = "urn:zeroship:file:/run/secrets/control-db"` - the
same table its operational siblings live in.
The overlay remains optional: absence continues to produce compiled defaults,
and the fixed `/etc/zeroship/zeroship.toml` discovery behavior is unchanged
(`crates/core/src/config/source.rs:52-104`).

All in-scope server and platform-one-shot environment names use the
`ZEROSHIP_` projection. Creator-CLI names are registered for raw-access
completeness but are not renamed by this server-config proposal. Names owned by
an outside convention, such as an operating-system ambient variable, do not
receive flags or TOML keys and must be declared as `External/ambient` with an
exact consumer and reason. They are not aliases for a canonical zeroship
setting.

### Worked examples from current settings

The following are target spellings. The evidence column points to each current
setting; it does not preserve the current spelling as an alias.

| Canonical | Class | Target flag | Target env | Target TOML | Current evidence |
| --- | --- | --- | --- | --- | --- |
| `control.port` | Operational | `--port` | `ZEROSHIP_CONTROL_PORT` | `[control] port` | `--port`/`CONTROL_PORT` at `crates/control/src/main.rs:42-44` |
| `control.database_url` | Secret | `--database-url-file` | `ZEROSHIP_CONTROL_DATABASE_URL` | `[control] database_url` | `--db`/`DATABASE_URL` at `crates/control/src/main.rs:50-66` |
| `gateway.database_url` | Secret | `--database-url-file` | `ZEROSHIP_GATEWAY_DATABASE_URL` | `[gateway] database_url` | `--db`/`DATABASE_URL` at `crates/gateway/src/main.rs:77-79` |
| `control_key` | Secret | `--control-key-file` | `ZEROSHIP_CONTROL_KEY` | top-level `control_key` | shared current readers are catalogued at `docs/reference/env-vars.md:228-245`; representative clap fields are `crates/control/src/main.rs:72-74` and `crates/gateway/src/main.rs:41-43` |
| `worker.max_pinned_isolates_per_app` | Operational | `--max-pinned-isolates-per-app` | `ZEROSHIP_WORKER_MAX_PINNED_ISOLATES_PER_APP` | `[worker] max_pinned_isolates_per_app` | `crates/worker/src/main.rs:70-76` |
| `gateway.signing_key` | Secret | `--signing-key-file` | `ZEROSHIP_GATEWAY_SIGNING_KEY` | `[gateway] signing_key` | current file-path input at `crates/gateway/src/main.rs:88-94` |
| `auth.postmark_webhook_password` | Secret | `--postmark-webhook-password-file` | `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_PASSWORD` | `[auth] postmark_webhook_password` | `crates/auth/src/config.rs:498-514` |
| `auth.public_url` | Operational | `--public-url` | `ZEROSHIP_AUTH_PUBLIC_URL` | `[auth] public_url` | `crates/auth/src/config.rs:348-360` |
| `observability.log_filter` | Operational | `--observability-log-filter` | `ZEROSHIP_OBSERVABILITY_LOG_FILTER` | `[observability] log_filter` | current independent flag/env at `crates/core/src/observability.rs:70-78` and TOML field at `crates/core/src/config/file.rs:173-183` |
| `migrated.provision_database_url` | Secret | `--provision-database-url-file` | `ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL` | `[migrated] provision_database_url` | `crates/migrated/src/main.rs:40-47` |

The transformation deliberately renames `--db` to the unambiguous
`--database-url-file`, and it deliberately removes `DATABASE_URL`,
`CONTROL_DATABASE_URL`, and `ZEROSHIP_DATABASE_URL`. Pre-launch policy requires
renaming and deleting old shapes in one change, rather than keeping deprecated
or compatibility aliases (`AGENTS.md:11-24`).

## 2. Supply-set classification

### AMENDED 2026-08-12: secrets sit with their siblings, and may be literals

Three linked changes replace the original `[secrets]` design. All three follow
from one observation: the section never enforced anything.

**Secrets are declared in their component's table, not a separate one.**
`SecretSection` is a flat struct of `Option<String>` fields
(`crates/core/src/config/file.rs:119-131`); the reference-only rule is enforced
per VALUE by `obtain_secret` / `is_secret_ref` operating on a raw string
(`crates/core/src/config/secrets.rs:451-470`), not by the section. Moving a
secret out of `[secrets]` therefore removes no check. It gains something the
flat section actively destroyed: LOCATION NOW ENCODES SHARING. A secret under
`[control]` is control's; a top-level secret is platform-global. Today
`control_key` (five consumers) and a per-service DSN sit side by side in one
flat table with nothing distinguishing them, which is the same ambiguity that
produced the live `ZEROSHIP_CONTROL_KEY` split - a literal in control, gateway,
worker and migrated, `${VAR}`-indirected only in auth
(`docs/reference/env-vars.md:99-108`).

**A secret may be a literal in the overlay.** The current rejection exists
because of the ARTIFACT, not the format: the code comment is "the config file
must never carry a plaintext secret", and `deploy/ops/zeroship.toml` is a
tracked file. That conflation blocks a standard deployment shape - projecting a
Kubernetes Secret as a mounted file and letting it BE the overlay, so the values
never enter git and are RBAC-controlled. It is also inconsistent with what is
already permitted: `Secret<T>` accepts a raw literal from the environment, and
under Kubernetes that value arrives from `secretKeyRef`, the same object with
the same lifecycle. Permitting a literal by env and forbidding it by mounted
file has no principled basis.

**The prohibition moves to tracked files** (Section 4.7). That is strictly
stronger: it fails at commit time, where the violation happens, instead of at
boot, after the secret is already in history.

What does NOT change: no secret value flag, so secrets never reach a process
argument list; automatic redaction, which becomes a prerequisite rather than a
nice-to-have once literals are permitted from more sources; and presence-only
`--check-config` output.

Cost accepted: an operator can no longer read the whole secret surface from one
table. That is recovered more accurately by `grep urn:` and by `--check-config`,
which reports what the process actually resolved rather than what the file
claims. Convention, not mechanism: keep secret entries last within each table.


Classification is type-driven, not a table of names and not a suffix heuristic.
Every declaration uses exactly one of these wrapper types:

```text
Operational<T>
Secret<T>
BootstrapControl<T>
CommandControl<T>
CommandEnv<Operational<T>>
CommandEnv<Secret<T>>
CliEnv<Operational<T>>
CliEnv<Secret<T>>
ExternalEnv<Operational<T>>
ExternalEnv<Secret<T>>
ExternalEnvFamily<T>
DevEnv<T>
TestEnv<T>
```

`Secret<T>` is mandatory when disclosure of the resolved value can do any of
the following:

1. authenticate or authorize a caller;
2. serve as private or symmetric key material for decryption, encryption,
   sealing, signing, or keyed verification;
3. spend money or act through a third-party provider account; or
4. disclose credentials through a URI/DSN grammar that permits userinfo or
   embedded tokens.

Everything else that changes server behavior is `Operational<T>`. Operational
URL types must reject credentials/userinfo; a URL type that permits credentials
is a secret type. Public identifiers, public keys, public JWKS URLs, bind
addresses, counts, durations, and feature policy values remain operational.
The field's wrapper is the only source-policy input; there is no
`secret = true/false` override for the author to make disagree with the type.

The macro derives the supply set mechanically from the wrapper:

- `Operational<T>` gets flag, env, TOML, and compiled-default slots. Its raw
  clap field is optional, so the compiled default cannot occupy and shadow a
  higher-precedence source. This retains the mechanism required by the ADR
  (`docs/decisions/2026-05-28-server-config-unification.md:39-44`).
- `Secret<T>` gets one environment literal/file-reference source, one generated
  CLI file-path flag, one TOML literal-or-reference slot at its canonical path,
  automatic redaction, and presence-only `--check-config` output. There is
  no secret value flag and no second `_FILE` environment alias; an environment
  may carry `urn:zeroship:file:...` when indirection is needed. A canonical env
  source may not contain `urn:zeroship:env:...`, because that would recreate an
  env-to-env alias. Contents read from a CLI or referenced file are the literal
  secret and are not recursively parsed as another reference. The deployable
  supply set is environment or file path. Vault and AWS reference syntax is
  parsed today, but startup returns `BackendUnavailable`
  (`crates/core/src/config/secrets.rs:232-277` and
  `crates/core/src/config/secrets.rs:335-386`); those backends do not become
  valid deployment sources merely because the example TOML can spell them.
- `BootstrapControl<T>` is for values needed before the overlay can be loaded,
  such as the overlay selector, and for safety controls that must not be
  persisted in the overlay. Its permitted sources are encoded by the type.
- `CommandControl<T>` covers actions such as config checking rather than a server
  setting. It has a flag only.
- `CommandEnv<Operational<T>>` and `CommandEnv<Secret<T>>` cover platform
  one-shot inputs that deliberately stay outside the server TOML overlay. They
  may have a generated flag and env name, retain the inner type's redaction
  rule, and never have a TOML path.
- `CliEnv<Operational<T>>` and `CliEnv<Secret<T>>` register an exact existing
  creator-CLI name with the typed accessor without deriving or renaming it,
  adding a server TOML layer, or otherwise bringing the creator CLI into this
  naming migration.
- `ExternalEnv<Operational<T>>` and `ExternalEnv<Secret<T>>` name an outside or
  ambient contract and record the exact process consumer. The inner wrapper
  preserves validation and redaction, but the supply set is only the outside
  environment name. An external name never silently doubles as a zeroship
  canonical setting.
- `ExternalEnvFamily<T>` is the explicit form for a protocol-owned dynamic
  prefix. It records the prefix, parser, and consumer; it cannot be used for a
  finite set of startup settings merely to escape canonical naming.
- `DevEnv<T>` has a canonical `ZEROSHIP_` env projection only, requires an
  explicit debug/dev compilation-mode consumer, and creates neither flag nor
  TOML input.
- `TestEnv<T>` is available only to test targets, has an exact test consumer,
  and contributes no production name or TOML path.

This also removes hand-maintained redaction as a separate correctness surface.
Today control has a custom `Debug` implementation with a manual redaction list
(`crates/control/src/main.rs:401-416`), worker has another
(`crates/worker/src/main.rs:179-213`), and `CheckConfigReport` accepts arbitrary
caller-selected `Plain` or `Secret` values
(`crates/core/src/config/bootstrap.rs:96-175`). Under this proposal,
`Secret<T>` cannot format its value, and generated report rows can only expose
presence and source kind.

## 3. Remove the deployment alias hop

Remove it. Do not preserve either direction as a compatibility name.

For service-specific DSNs, Compose will use identical host-substitution and
container names:

```yaml
control:
  environment:
    ZEROSHIP_CONTROL_DATABASE_URL: ${ZEROSHIP_CONTROL_DATABASE_URL:-postgres://...}
gateway:
  environment:
    ZEROSHIP_GATEWAY_DATABASE_URL: ${ZEROSHIP_GATEWAY_DATABASE_URL:-postgres://...}
worker:
  environment:
    ZEROSHIP_WORKER_DATABASE_URL: ${ZEROSHIP_WORKER_DATABASE_URL:-postgres://...}
```

Each binary declares that exact environment projection. When env supplies the
secret, the TOML secret key is omitted because an env reference back to the
same env would be redundant: the direct env tier already wins. An env reference
to any other name would be the alias hop under another spelling, so parsed ops
TOML rejects env references altogether. Operators who want file indirection put
`urn:zeroship:file:...` at the derived
`[<service>] database_url` path. File resolution is implemented today
(`crates/core/src/config/secrets.rs:335-386`); the parsed but unavailable Vault
and AWS forms are not part of this proposal's supply set
(`crates/core/src/config/secrets.rs:232-277`).

Shared secrets use the same rule. Compose assigns
`ZEROSHIP_CONTROL_KEY: ${ZEROSHIP_CONTROL_KEY:-...}` to each consumer, each
consumer declares `ZEROSHIP_CONTROL_KEY`, and an optional TOML locator is
`[secrets] control_key`. There is no `CONTROL_KEY` fallback.

Removing the hop has four benefits:

1. The name an operator sets is the name the binary declares.
2. Role-specific DSNs are explicit fields rather than different values hidden
   behind one per-container alias. The current file explains that the shared
   alias resolves to different role-specific DSNs
   (`deploy/ops/zeroship.toml:53-60`).
3. The TOML overlay returns to being an optional source, not mandatory glue
   between two environment namespaces.
4. A direct equality gate can reject any future `LEFT: ${RIGHT}` platform
   alias instead of trying to document its precedence.

This is an atomic rename. The old host knobs, container names, TOML entries,
clap env names, tests, and documentation are deleted or updated together. No
binary reads both names, and Compose never supplies both.

## 4. Enforcement

### 4.1 One declaration generates every spelling

Add `crates/core/src/config/names.rs` for `CanonicalName`, source wrappers,
`ConfigSpec`, transforms, overlay lookup, and the typed environment accessor.
Add a small local proc-macro crate, `crates/config-macros`, and re-export its
attribute from core. A local proc-macro is an implementation aid, not a new
configuration stack: the repository already has a workspace proc-macro crate
using `syn`, `quote`, and `proc-macro2`
(`crates/runtime-macros/Cargo.toml:1-13`), while core already depends on clap,
serde, and toml (`crates/core/Cargo.toml:7-12`).

A declaration has this conceptual shape:

```rust
#[zeroship_config(binary = "control")]
struct ControlSources {
    #[config(name = "control.port", default = 9090)]
    port: Operational<u16>,

    #[config(name = "control.database_url")]
    database_url: Secret<DatabaseUrl>,
}
```

The attribute emits separate raw source carriers and one resolved field. For
`Operational<T>`, the clap carrier has the derived long flag and derived env
metadata. For `Secret<T>`, the clap carrier is an `Option<PathBuf>` with the
derived `-file` long flag and no clap env metadata; the generated resolver reads
the separately derived canonical env name through the typed accessor. This is
necessary because one clap field cannot parse a CLI path and an environment
secret value as different types. The CLI file content is passed to `T` as
literal secret material, while env and TOML references are handled under the
source-specific rules in Section 2.

Every generated resolver also emits a `ReadSite` registration for its
`(canonical identity, consumer, source)` tuple. Manually accessed `CliEnv`,
`ExternalEnv`, `ExternalEnvFamily`, `DevEnv`, and `TestEnv` keys are constructed
only by an accessor macro that emits the same registration at the call site;
the key wrapper's constructor is private. The compiled contract rejects both a
read site without a declaration and a declared `(key, consumer)` with no read
site. Deleting the last reader while leaving its declaration therefore fails
CI, which is the exact set-but-unread property the deployment catalogue alone
cannot provide.

The attribute also emits the TOML lookup path, resolved-field code, redaction
behavior, and a `&'static [ConfigSpec]`. Field declarations may still carry
parsers, validators, conflicts, and help text, but may not carry literal
`long`, `env`, or serde `rename` spellings. The optional overlay is parsed with
serde/toml and walked by canonical path; unknown leaf paths fail, preserving
the current deny-unknown posture (`crates/core/src/config/file.rs:40-60`).

`ConfigSpec` is keyed by canonical identity and contains a consumer set. A
shared setting is declared once and consumer fields refer to that spec, for
example `#[config(shared = CONTROL_KEY)]`; they do not repeat its string name.
Registry coalescing permits multiple consumer fields only when canonical name,
class, resolved type, validator, default, env projection, and TOML semantics
are identical. TOML-path uniqueness is checked between canonical identities,
not between consumers of the same identity.

### AMENDED 2026-08-12: consumers agree on identity, never on default

The paragraph above is wrong in one clause and was unbuilt in another. Both are
corrected here, and both are now enforced by code rather than described.

**The default is removed from the coalescing rule.** Requiring every consumer of
one identity to agree on its DEFAULT has no satisfiable form for the settings
this design already governs. `observability.log_filter` defaults to
`info,zeroship_control=debug` in control and `info,zeroship_gateway=debug` in
the gateway, because the directive names the declaring crate. There is no single
value that is correct for every binary: `info,zeroship_control=debug` applied to
the gateway enables nothing. As written, the rule would have rejected the five
declarations Step 2 landed, and Step 7 turns the rule into a build-blocking
gate, so the contradiction would have surfaced as a blocked build rather than as
a design question.

Splitting the identity instead - `control.log_filter`, `gateway.log_filter` -
is worse, not a workaround. It gives one concept six operator-visible
environment names and destroys "set the log level once for the deployment,"
which is the drift this proposal exists to remove.

The amended rule: **consumers of one canonical identity must agree on canonical
NAME, SUPPLY CLASS, RESOLVED TYPE and SUPPLY SET. They may differ on the
compiled default.** Every property in that list is something an operator is
misled by, because one `ZEROSHIP_*` spelling and one overlay path must mean one
thing everywhere. The compiled default is invisible from outside the process and
is a per-binary behaviour, not part of the operator-facing contract.

**`#[config(shared = ...)]` is built, narrower than sketched.** Step 2 shipped
without it, so each of the five servers repeated the canonical string and the
wrapper type: `name = "config"` appeared five times, and six identities were
spelled thirty times between them. That repetition made two failures
representable and neither detectable by any gate in Section 4:

- A TYPO de-shares the identity. `observability.log_filtr` in one binary is a
  new canonical name with a unique env projection, a unique flag and a matching
  read site, so the collision check, the projection check and the
  declared-equals-read check all pass while that service silently ignores the
  operator's variable. This is the proposal's own failure mode, reintroduced
  thirty times.
- A DIVERGENT wrapper or inner type gives one environment variable two parse
  behaviours, and for `Secret` against `Operational` two different redaction
  rules. The existing collision check skips same-name pairs, so nothing looked.

The implemented form names the identity by SYMBOL and reads the canonical name,
supply class and resolved type from a table in `crates/config-macros/src/shared.rs`:

```rust
#[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
pub log_filter: Operational<String>,
```

An unknown symbol does not compile and the diagnostic lists the known ones. A
field whose declared type differs from the table's does not compile. And
`#[config(name = "...")]` REFUSES a string that matches a shared canonical name,
which is what keeps the table the single spelling rather than a second one -
the distinction from the manifest rejected in Section 6, where the objection is
that a second spelling can be edited to agree with a typo.

It is narrower than the sketch in two ways worth stating. It does not share the
FIELD, only the identity: each binary still declares the field, its help text
and any extra `#[arg]` attributes, because those legitimately differ (migrated's
`--check-config` help says "without provisioning, connecting or listening";
worker adds `worker.workflow_advance_unsigned` next to the shared block). And
the type check is textual, since a proc macro cannot resolve a path to a type;
it guarantees that every consumer writes the same tokens, not that those tokens
name the same type.

The registry-level coalescing check is implemented too, in
`validate_contract`, as defence in depth for hand-written `ConfigSpec` values
that the attribute never sees. It compares class, type, environment projection
and supply set, and deliberately does not compare defaults.

Move each command type into a library-visible config module so a checker can
link it: new `src/config.rs` modules in control, gateway, worker, migrated, and
workflow-scheduler, plus auth's existing exported config module
(`crates/auth/src/lib.rs:1-6`). Each exports its
generated clap source type and `ConfigSpec` slice; `main.rs` only invokes that
surface. The present parser locations are
`crates/control/src/main.rs:38-399`,
`crates/gateway/src/main.rs:25-204`,
`crates/worker/src/main.rs:26-177`,
`crates/migrated/src/main.rs:20-114`,
`crates/workflow-scheduler/src/main.rs:13-52`, and
`crates/auth/src/config.rs:31-533`.

The platform migration one-shot also moves its manual parser into a linkable
`crates/zeroship-migrate-adapter/src/platform_migrate_config.rs` module behind
the existing `platform-cli` feature and replaces it with a generated clap
`CommandEnv<Secret<DatabaseUrl>>` source. It has no TOML layer, but its flag and
canonical env name remain contract-checked. The bin already requires that
feature (`crates/zeroship-migrate-adapter/Cargo.toml:30-34` and
`crates/zeroship-migrate-adapter/Cargo.toml:60-62`); its present manual parser and raw fallback are at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:73-113`.

Add `crates/config-contract` as a non-shipped workspace dev tool with both a
library and a `zeroship-config-contract` binary. The workspace glob includes
`crates/*` (`Cargo.toml:1-3`), and CI already runs workspace check and test
commands (`.github/workflows/ci.yml:5-29`). The tool links the exported config
modules and invokes `CommandFactory::command()` for each generated clap source.
Its compiled test asserts:

1. every non-built-in value-bearing clap argument has exactly one generated
   `ConfigSpec` source;
2. each operational argument's actual long flag and clap env metadata equal
   its projections;
3. each secret argument's actual long flag equals its `-file` projection and
   has no clap env metadata, while its `ConfigSpec` carries exactly the derived
   env projection;
4. every remaining non-built-in argument is explicitly `BootstrapControl` or
   `CommandControl`, or is a declared `CommandEnv` source, with unavailable
   layers absent; clap's built-in help and version actions are the only
   automatic exemptions;
5. operational and secret TOML paths are derived, type-compatible, and unique
   between canonical identities;
6. canonical identities, global env projections, and per-binary flag
   projections have no collisions, including a negative fixture for each;
7. all in-scope platform env names begin `ZEROSHIP_`;
8. every consumer of a shared identity agrees on its canonical name, class,
   resolved type and supply set - but NOT on its default (AMENDED 2026-08-12;
   see the amendment above); and
9. declared source/consumer pairs and compiled `ReadSite` pairs are equal in
   both directions, except generated sources disabled by a declared target cfg.

The anti-vacuity set is derived from `cargo metadata --no-deps`, not a static
expected-binary list. Every workspace `bin` target must have either a compiled
config registration or an explicit target-specific package-metadata
classification such as creator CLI, test/dev tool, or build helper, with a
reason. Production platform one-shots, including platform migration, require
registration and cannot use an out-of-scope classification. Adding an
unclassified target fails. The proc-macro expansion tests
prove the transforms; the compiled clap test proves that the transforms
survived macro composition. There is no parallel hand-maintained manifest of
configuration names.

### 4.2 TOML URN and consumer gate

In `crates/config-contract/tests/ops_toml.rs`, load both
`deploy/ops/zeroship.toml` and `deploy/ops/zeroship.example.toml`. Check both
the raw text and the parsed TOML. Assert:

1. every leaf is a generated TOML path with at least one production consumer;
2. every secret leaf sits at its canonical path, and where it is a reference it
   uses a backend implemented at startup, currently `urn:zeroship:file:`. A
   LITERAL is permitted here and is checked instead by the tracked-file gate in
   Section 4.7, which is where the no-plaintext guarantee now lives;
3. a lexical scan of every concrete `urn:zeroship:env:NAME`, including comments,
   finds a generated environment name with a production consumer;
4. if that env URN is a parsed value, it is under the secret path for that same
   `ConfigSpec`, after which the final policy rejects it as redundant aliasing;
5. no operational leaf contains a secret reference; and
6. both expected files, the parsed-leaf walk, and a valid env-URN unit fixture
   were examined, so a broken traversal cannot pass at zero.

The exact-key assertion catches a stale schema field even when its env target
is read for another purpose. The exact-spec assertion catches the Postmark
case. The current format-only validator intentionally cannot make either
assertion (`crates/core/src/config/secrets.rs:389-403`). Include checker unit
fixtures containing an unknown key, a dead env target, a target owned by the
wrong setting, a parsed env-to-env alias, and an unavailable backend; each must
fail. The unavailable-backend fixture also prevents the current example's
Vault/AWS placeholders from looking deployable while their resolvers return
`BackendUnavailable` (`deploy/ops/zeroship.example.toml:77-114` and
`crates/core/src/config/secrets.rs:335-386`). Generic documentation placeholders
such as `<VARNAME>` are not concrete names and are excluded from the lexical
match.

### 4.3 Set-but-unread gate for deployments

Add `tests/config_name_alignment_gate.sh`, and invoke it as a named step in
`.github/workflows/ci.yml` next to the existing Compose/deploy gates at
`.github/workflows/ci.yml:246-274`. The shell wrapper invokes the compiled
checker directly, so it cannot consume a stale generated name file:

```bash
umask 077
gate_tmp="$(mktemp -d)"
trap 'rm -rf "$gate_tmp"' EXIT
docker compose -f deploy/compose/docker-compose.yml \
  config --no-interpolate --format json > "$gate_tmp/source.json"
docker compose -f deploy/compose/docker-compose.yml \
  config --format json > "$gate_tmp/rendered.json"
cargo run --quiet -p zeroship-config-contract -- check-compose \
  --source-json "$gate_tmp/source.json" \
  --rendered-json "$gate_tmp/rendered.json"
```

The gate requires Compose's parser to normalize map syntax, list syntax, and
YAML anchors into the two JSON models; Step 1's fixtures must prove that the
pinned Compose version preserves uninterpolated scalar expressions. `env_file`
is rejected for platform services because its keys are otherwise outside this
checked model. For each platform service, every explicit container environment
key must be declared as consumed by that exact binary, or as an exact
external/ambient input for that binary. A name consumed by another service
does not count. A bare list entry such as `NAME` is treated as host pass-through
and must still be a declared name.

When the proven non-interpolated model contains an environment value whose
entire scalar is exactly one Compose interpolation, such as `${RIGHT}` or
`${RIGHT:-default}`, require `LEFT == RIGHT`. Composite values such as a public
URL assembled from scheme, domain, and literals are not one-to-one aliases and
are excluded from this equality rule; their container `LEFT` name is still
consumer-checked. Shared settings must use the same raw assignment expression
for every consumer. These rules make the current
`CONTROL_DATABASE_URL -> ZEROSHIP_DATABASE_URL` shape and divergent shared-key
defaults unrepresentable.

The checker identifies in-scope services by equality between compiled
deployment registrations and services using the platform image/build, rather
than a duplicated shell list. It fails if either set is empty or differs. Its
fixture suite mutates an unknown key, wrong-service key, left/right alias,
mapping-form environment, list-form environment, YAML anchor, and `env_file`;
each mutation must reach the expected pass or failure. Diagnostics print
service and variable names only, never rendered values. The `umask` and cleanup
keep rendered secrets out of world-readable files and logs.

This gate's set-but-unread guarantee covers variables supplied to a checked-in
platform service. It cannot see an arbitrary extra variable that exists only in
an operator's host shell or `.env` and is never forwarded by Compose.

### 4.4 Set-but-unread check at process startup

Static CI only covers checked-in deployments. At `bootstrap`, enumerate names
with the reserved `ZEROSHIP_` prefix and compare them with the current binary's
generated consumer set. An unknown or known-but-other-binary name is a config
error in both real startup and `--check-config`. Report names only, never
values. This catches a typo in an external deployment that CI cannot see.

External/ambient unprefixed names cannot be discovered safely by prefix, so
they remain exact declared inputs and are covered by the rendered-deployment
gate. No unknown unprefixed environment variable is assumed to be zeroship
configuration.

### 4.5 Ban undeclared raw environment reads

Use the compiler as the primary gate. Add `clippy.toml` disallowed-method
entries for `std::env::{var,var_os,vars,vars_os}`, set workspace
`clippy::disallowed_methods` to `deny`, and permit local allows only in
`crates/core/src/config/env.rs` and the sealed `libs/*` test accessors defined
below. The workspace already centralizes Clippy policy
in `Cargo.toml:242-250` and CI runs Clippy across workspace targets at
`.github/workflows/ci.yml:437-451`. A required fixture imports
`std::env::var` as another name and proves the compiler-resolved lint still
rejects the call.

Add `crates/config-contract/tests/workspace_lints.rs`. It obtains every
first-party workspace member and feature from `cargo metadata`, parses each
member manifest, and requires `[lints] workspace = true`. There is no per-crate
opt-out. Its mutation fixture adds a workspace member without lint inheritance
and proves the gate fails. This prevents a new crate from silently escaping the
compiler rule.

Add `crates/core/tests/config_env_access_gate.rs` as the second line. It
enumerates tracked Rust files using `git ls-files`, following the repository's
existing source-gate pattern
(`crates/core/tests/source_is_greppable_test.rs:85-111`), and rejects any
direct `std::env` call, import or re-export of those four methods, dynamic
dispatch to them, `allow(clippy::disallowed_methods)` outside the central
or sealed library-test accessors, known direct environment FFI escape paths,
and zeroship-owned
`env!`/`option_env!` literals. Because this parses tracked source independent of
cfg activation, a raw read hidden behind a disabled feature still fails. A
fixture places a raw read behind an otherwise disabled feature and proves that
fallback.
Standard compiler/build inputs such as `CARGO_*` and `OUT_DIR` are explicit
build-time exemptions because they are not process startup configuration.
Macro-generated zeroship config accesses must call the typed accessor; reads
inside third-party dependencies are outside the first-party source gate.

The compiler and source checks cover all first-party Rust, including the
creator CLI and tests, and exclude vendored dependencies. The creator CLI uses
`CliEnv<...>` only to make its reads visible; it still receives no server TOML
surface. Code under `crates/*` uses `TestEnv<T>`, `DevEnv<T>`,
`ExternalEnv<...>`, or a generated production spec as appropriate.

The publishable `libs/*` drivers remain zeroship-independent, as required by
their documented boundary (`AGENTS.md:141-148`); they do not gain a core
dependency. Production library APIs accept resolved options rather than read
process config. A library test may use a dependency-free, sealed local test-key
enum in exactly `libs/<crate>/tests/common/env.rs`; the source gate recognizes
that path pattern, requires literal enum variants, and permits raw access only
inside its private accessor. A fixture proves a production library read and a
test read outside that module both fail. Platform processes declare and read
any external credentials before injecting them into a library.

The central accessor accepts a typed key and consumer, never `&str`, and records
the read against that consumer. That requirement brings helper-based reads such
as metering's `env_nonempty(key)` into the contract rather than allowlisting the
helper. Compile-fail tests reject an undeclared key and a key read by the wrong
binary. A source-gate self-test inserts an illicit allow attribute and proves it
fails.

### 4.6 `--check-config` coverage

Move migrated onto `bootstrap_or_exit`, add the generated overlay/bootstrap
controls and structured report, and exit before non-config filesystem
mutations and before database, listener, or network side effects. Extend
`tests/config_check_e2e.sh` to build and exercise
worker and migrated as well as control, gateway, and auth; the current build
and binary list covers only the latter three
(`tests/config_check_e2e.sh:181-199`). Add the script as a named CI step.

The E2E must assert all five server names (AMENDED 2026-08-12: five, not six -
the scheduler has no `--check-config` and its `main` exits 1, so it cannot be
driven by this script until the extraction described in the scope section
lands), each resolved source, secret
presence-only output, CLI-over-env-over-file precedence, and rejection of an
unknown `ZEROSHIP_*` name. Retain a minimum assertion floor; the current test
already uses one to prevent a zero-assertion pass
(`tests/config_check_e2e.sh:375-397`).

### 4.7 No plaintext secret in a tracked file

ADDED 2026-08-12 with the Section 2 amendment. Because a secret may now be a
literal in the overlay, the guarantee that used to come from rejecting TOML
literals is restored here, against the artifact that actually needs it.

A repository gate walks every TRACKED file that can carry configuration -
`deploy/**/*.toml`, `deploy/**/*.yml`, and any `*.env` under version control -
resolves each value against the canonical registry, and fails when a value
whose canonical identity is secret-classed is not a `urn:`/`arn:` reference.
Classification comes from the registry, so the gate needs no name heuristics
and cannot drift from the declarations.

This is strictly stronger than the runtime check it replaces. The runtime
rejection only fires if someone deploys the mistake; the repository gate fails
at commit time, before the secret enters history, which is the point at which a
leaked credential becomes unrecoverable.

Two further protections, neither of which the format rule provided:

- Runtime permission check. When the overlay contains a secret literal and is
  group- or world-readable, startup refuses. This mirrors OpenSSH's treatment
  of private key files and targets the realistic misconfiguration - a mounted
  secret left at mode 0644 - rather than the file's syntax.
- Redaction becomes load-bearing rather than incidental. `Secret<T>` cannot
  format its value, replacing the hand-maintained redaction lists in
  `crates/control/src/main.rs:401-416` and `crates/worker/src/main.rs:179-213`.
  With literals permitted from more sources, a `Debug` that printed a raw value
  would be a live disclosure, so this part of Section 4.1 must land with the
  amendment and not after it.

WHAT THIS DOES NOT COVER: a secret literal in an UNTRACKED overlay is
deliberately allowed - that is the Kubernetes mounted-Secret case the amendment
exists to permit - so the gate says nothing about files it cannot see. It also
does not inspect image layers or Compose files rendered at deploy time outside
the repository.

## 5. Migration plan ordered by risk

All steps belong to one pre-launch change set. Intermediate commits may be used
for review, but no mismatched producer/consumer state is deployable and no old
name remains as an alias.

### Step 1: Add inert naming machinery and negative fixtures

Risk: low. Add `crates/config-macros`, `crates/config-contract`, and
`crates/core/src/config/names.rs`; give the contract crate its library and
checker-binary targets, but export no changed runtime names yet. Add transform,
canonical/env/flag/TOML collision, reserved-`secrets`, bad-URN,
wrong-consumer, raw-read, and empty-extraction tests. Touch the workspace
dependency table and core module exports; the existing config module export
surface is centralized at
`crates/core/src/config/mod.rs:13-34`.

Blast radius: new tooling files, root `Cargo.toml`,
`crates/core/Cargo.toml`, `crates/core/src/config/mod.rs`, and the package
manifests that classify each existing bin target for the cargo-metadata
anti-vacuity check. No deployment caller changes.

### Step 2: Convert bootstrap and command controls, including migrated

Risk: low to medium. Generate `config`, `no-config`, `check-config`,
`check-config-format`, observability, and safety controls. Add migrated to the
shared bootstrap/report path. Preserve optional discovery and current
precedence. Move command definitions from binary-only modules into the exported
config modules named in Section 4.1 so the compiled checker can invoke them.

Blast radius: `crates/core/src/config/{bootstrap,source,env}.rs`,
`crates/core/src/observability.rs`, the config/parser portions of
`crates/{control,gateway,worker,migrated}/src/main.rs`,
new `crates/{control,gateway,worker,migrated,workflow-scheduler}/src/config.rs`,
their `src/lib.rs` module exports, `crates/auth/src/{config,main}.rs`, and
`crates/zeroship-migrate-adapter/{Cargo.toml,src/lib.rs}`, new
`crates/zeroship-migrate-adapter/src/platform_migrate_config.rs`, its platform
migration bin, and `tests/config_check_e2e.sh`. The current shared-bootstrap call sites are at
`crates/control/src/main.rs:630-645`,
`crates/gateway/src/main.rs:277-291`,
`crates/worker/src/main.rs:307-320`, and
`crates/auth/src/main.rs:24-51`; migrated's direct path is
`crates/migrated/src/main.rs:116-125`.

### Step 3: Convert operational values

Risk: medium. Move each operational clap field and overlay lookup to
`Operational<T>`, apply canonical target names, and update every flag caller in
Compose, tests, examples, and runbooks in the same patch. Add TOML sections only
for values actually consumed; an absent section remains valid.

Blast radius: parser structs in `crates/control/src/main.rs:38-399`,
`crates/gateway/src/main.rs:25-204`,
`crates/worker/src/main.rs:26-177`, and
`crates/auth/src/config.rs:31-533`; migrated at
`crates/migrated/src/main.rs:20-114`; scheduler at
`crates/workflow-scheduler/src/main.rs:13-52`; and the new exported config
modules introduced in Step 2. Before editing, run
`cargo run --quiet -p zeroship-config-contract -- inventory --format tsv` to
emit each current flag/env/TOML spelling, consumer, declaration path, and every
tracked literal call site. Canonical and target columns are populated as fields
are converted, so the before/after output becomes the per-PR migration
checklist. Treat it as a review artifact, not a checked-in second manifest.
Every row must be converted or explicitly classified before this step closes.
This is a source rename, not an alias phase.

### Step 4: Centralize raw environment reads

Risk: medium. Replace every first-party raw variable read with a typed config,
CLI, test, build, or external key. Pass resolved settings into inner
runtime/plugin code
where the value is zeroship startup config; do not let inner modules rediscover
it from process-global state. Turn on the Clippy and source gates only after
the last read is converted. The current dynamic env-reference read in
`crates/core/src/config/secrets.rs:354-359` remains a tracked blocker until
Step 5 deletes env-to-env secret references; it is not allowlisted in the final
gate.

Blast radius includes root `Cargo.toml`, new `clippy.toml`,
`crates/core/tests/config_env_access_gate.rs`, every first-party package
manifest needed to make workspace-lint inheritance mandatory, and current
representative sites in
`crates/worker/src/handler.rs:1700-1701`,
`crates/control/src/http_util.rs:157-159`,
`crates/runtime/src/node/net/connect.rs:394-438`,
`crates/metering/src/outbox.rs:88-103`, and every other site found by the
inventory command from Step 3. The checklist also includes core's current raw
access boundary (`crates/core/src/config/env.rs:10-20`) and creator-CLI reads as
`CliEnv<...>` registrations, without adding CLI TOML support. Dynamically
constructed reads require manual inspection
because the documented text scan cannot see them
(`docs/reference/env-vars.md:12-19`).

### Step 5: Convert secrets and delete value flags

Risk: high. Replace secret-bearing `String`/`Option<String>` fields with
`Secret<T>`, generate only path flags, move direct values to canonical env
names, generate presence-only reports, and replace free-form `obtain_secret`
labels with `ConfigSpec` identity. Verify every secret validator still runs on
the resolved value and that `--check-config` performs source-policy and format
validation without opening files or fetching secrets. Delete the
`SecretRef::Env` parser/resolver arm and its tests, rather than retaining a
general env-to-env alias facility. Delete the parsed-but-unavailable Vault and
AWS variants at the same time, so the type admits exactly literal input and
file indirection. The current parser and resolver are at
`crates/core/src/config/secrets.rs:232-386`.

Blast radius: `crates/core/src/config/{file,secrets,bootstrap}.rs`; secret
parsers and resolution blocks at `crates/control/src/main.rs:694-820`,
`crates/gateway/src/main.rs:306-373`,
`crates/worker/src/main.rs:328-380`, and
`crates/auth/src/main.rs:324-445`; migrated's DSNs/keys at
`crates/migrated/src/main.rs:31-55` and `crates/migrated/src/main.rs:98-105`;
the platform migration one's manual DSN flag/read at
`crates/zeroship-migrate-adapter/src/bin/zeroship-platform-migrate.rs:73-105`;
its generated source in
`crates/zeroship-migrate-adapter/src/platform_migrate_config.rs` and exports in
that crate's `Cargo.toml`/`src/lib.rs`;
and every secret call site emitted by the Step 3 inventory. This step also
replaces or removes unavailable Vault/AWS examples at
`deploy/ops/zeroship.example.toml:77-114`. Hand-written `Debug`
implementations are deleted only after generated redaction tests pass.

### Step 6: Remove aliases and rewrite deployment inputs atomically

Risk: high because a missed producer causes startup failure. Rewrite
`deploy/ops/zeroship.toml`, `deploy/ops/zeroship.example.toml`, and
`deploy/compose/docker-compose.yml` to canonical projections. Delete redundant
env-backed secret TOML references. Update all shell suites, examples, and
runbooks found by exact old-name searches. Do not add fallback reads. For each
row in the generated migration inventory, search the entire tracked tree for
its old flag, env, and TOML spellings and resolve every hit. The proposal itself
may retain old spellings as historical evidence. A hit that belongs to the
renamed identity must change; an identical token that belongs to a genuinely
separate creator-CLI, test, or external contract must be explicitly classified
in the generated checklist. No renamed identity retains an old spelling.

For the measured middle aliases, run and resolve every result from:

```bash
git grep -n -E \
  'ZEROSHIP_DATABASE_URL|CONTROL_DATABASE_URL|GATEWAY_DATABASE_URL|WORKER_DATABASE_URL|DATABASE_URL' \
  -- . ':!docs/proposals/2026-08-11-config-name-alignment.md'
git grep -n -E \
  'ZEROSHIP_CONTROL_KEY|CONTROL_KEY|ZEROSHIP_MASTER_KEY|ZEROSHIP_WORKER_KEY' \
  -- . ':!docs/proposals/2026-08-11-config-name-alignment.md'
```

The current live aliases are at `deploy/ops/zeroship.toml:65-75` and service
assignments at
`deploy/compose/docker-compose.yml:200-217`,
`deploy/compose/docker-compose.yml:373-394`, and
`deploy/compose/docker-compose.yml:466-472`.

### Step 7: Enable all mandatory gates and regenerate the reference

Risk: high because an incomplete registry or deployment classification blocks
every build or boot at activation. Enable the compiled contract, TOML, source,
Compose, runtime, and
five-binary E2E gates in the same change that removes the last old name.
Regenerate `docs/reference/env-vars.md` from the generated contract for
zeroship-owned settings, retaining a separate declared external/test section.
Keep the extraction command as an audit that must equal the generated set,
not as the source of truth.

Blast radius: `.github/workflows/ci.yml`, `tests/config_name_alignment_gate.sh`,
`tests/config_check_e2e.sh`, `crates/config-contract`, and
`docs/reference/env-vars.md`. Deployment is permitted only after all five
servers pass `--check-config` against the rendered environment and mounted
overlay.

## 6. Rejected alternatives

### Keep config-rs, figment, or confique in reserve

Rejected. The accepted ADR already chose plain clap, serde, and toml after
evaluating them (`docs/decisions/2026-05-28-server-config-unification.md:60-63`).
Name generation and contract validation do not require a new merge framework.

### Make TOML required or primary

Rejected. The file remains an optional overlay and the missing well-known path
remains a normal all-default state (`crates/core/src/config/source.rs:52-104`).
The accepted ADR rejected a required file because existing dev/deploy flows are
CLI-driven (`docs/decisions/2026-05-28-server-config-unification.md:64-65`).

### Keep the alias hop because each service needs a different DSN

Rejected. Different DSNs require different canonical identities, not a shared
middle name. Service-scoped canonical names preserve role separation directly.
Keeping the hop also preserves the possibility of supplying two disagreeing
names; the catalogue already warns that the current direct env and file-ref
families can both be set and resolve by precedence
(`docs/reference/env-vars.md:23-48`).

### Preserve old short names as explicit aliases

Rejected. The local-scope transform already keeps useful flags short. Any
additional `--db`, `DATABASE_URL`, or old env fallback would be a second source
name and would recreate the problem. Pre-launch policy explicitly rejects
deprecation aliases and migration shims (`AGENTS.md:11-24`).

### Keep operational TOML limited to today's hand-selected fields

Rejected. That would leave TOML spelling outside the canonical contract for
most operational values, and a future overlay addition could drift before a
gate knew the path existed. Making every operational path derivable does not
make the optional file mandatory; it only makes the source available and
checkable.

### Use a checked-in name manifest beside the Rust declarations

Rejected. A hand-maintained manifest can be edited to agree with a typo while
the actual clap command still differs. The compiled clap command and generated
TOML projection must be compared to metadata emitted from the declaration that
parses the value.

### Use grep as the contract

Rejected. The catalogue itself says concatenated names are invisible
(`docs/reference/env-vars.md:12-19`), and metering currently passes names through
a string-taking helper (`crates/metering/src/outbox.rs:88-103`). Grep remains a
useful audit and a ban on raw access, but generated compiled metadata is the
authority.

### Warn about unknown environment names but do not fail CI/startup

Rejected. A warning does not prevent the exact set-but-unread deployment shape
this proposal addresses. The static gate fails checked-in deployments, and
bootstrap fails unknown reserved-prefix names before server startup.

### Put secret values in TOML or retain secret value flags

Rejected. Existing secret resolution already rejects TOML literals
(`crates/core/src/config/secrets.rs:451-470`), and the target policy permits
only env values and file-path locators. A path flag preserves operational
ergonomics without exposing secret material in a process command line.

## 7. Case against this proposal

The strongest case for doing less is that the observed failures are narrower
than the proposed mechanism. A small test could compare every current
`urn:zeroship:env:` target with the current clap/raw-read union, another could
compare rendered Compose keys with per-service readers, and a focused change
could add migrated's missing `--check-config`
(`crates/migrated/src/main.rs:20-119`). The current TOML structs already
reject unknown keys (`crates/core/src/config/file.rs:40-60`), current clap
parsers already provide typed validation
(`crates/control/src/main.rs:38-110` and
`crates/auth/src/config.rs:31-60`), and the current overlay loader has
clear optional/discovery semantics (`crates/core/src/config/source.rs:52-104`).
On that view, a dotted canonical namespace, wrapper types, a proc macro, and a
workspace contract crate are a bespoke mini-framework built to prevent a class
of error that two or three narrow CI checks could catch.

The migration also has real synchronized-outage risk. It changes flags, env,
TOML, Compose, tests, and docs at once. Secret delivery is the least forgiving
part: a missed producer prevents boot, while an incorrectly converted
redaction path can disclose a credential. The current manual redaction burden
is visible in control and worker
(`crates/control/src/main.rs:401-416` and
`crates/worker/src/main.rs:179-213`); replacing it is beneficial only if the
generated replacement is tested more rigorously than the handwritten code.

Doing less would be the right call if all of the following were true:

1. every real deployment manifest were repository-controlled and covered by
   the rendered-Compose gate;
2. no external operator, Helm, systemd, or sibling-repository environment
   supplied these binaries;
3. raw environment access were frozen behind one typed accessor;
4. TOML remained a very small, rarely extended surface; and
5. the narrow gates inspected compiled clap metadata and had mutation/self-tests,
   rather than trusting text extraction.

Under those conditions, the narrow gates would close the known failures at far
lower migration cost. This proposal chooses the broader mechanism because the
current surface is already split between independently named clap fields, a
handwritten TOML schema, free-form secret labels, deep raw reads, and deployment
template aliases (`crates/core/src/config/file.rs:40-171`,
`crates/core/src/config/secrets.rs:451-481`,
`crates/runtime/src/node/net/connect.rs:394-438`, and
`deploy/compose/docker-compose.yml:200-217`). That conclusion is an engineering
judgment based on the opened evidence, not a measured proof that another drift
would occur.

## 8. What was not verified

- I did not build or execute any Rust binary, start the Compose stack, run
  `--check-config`, or connect to a live deployment. All current-state claims
  above come from the cited source and deployment files. In particular, I did
  not independently repeat the live command-line inspection described in the
  task input.
- I did not prototype the proposed attribute macro. Its ability to emit the
  exact clap/overlay companion types is a design inference. Step 1 must prove
  expansion and compiled-command behavior before parser conversion begins.
- I did not classify every current setting into a final canonical name. The
  worked examples exercise shared, service-specific, operational, secret,
  file-backed, and cross-domain transforms; implementation still needs a
  reviewed full inventory.
- I did not inspect every deployment variant, systemd unit, Helm chart, sibling
  repository, or host `.env`. The deployment gate described here is grounded in
  `deploy/compose/docker-compose.yml`; claims about outside deployments would be
  inference.
- I did not run the proposed two-form Compose render or verify the minimum
  deployed Compose version's `--no-interpolate --format json` behavior. Step 1
  must pin and fixture-test that interface before the gate is relied upon.
- I did not prototype the Clippy `disallowed_methods` rule or verify its
  behavior through an imported alias. The compiler fixture, universal
  workspace-lint-inheritance gate, and cfg-independent source fallback are
  design requirements, not measured current behavior.
- I did not inspect `zeroship-sandbox`. The sandbox remains
  outside the accepted server-config scope
  (`docs/decisions/2026-05-28-server-config-unification.md:56-58`).
- I did not verify that every URI currently typed as a plain string rejects
  embedded credentials. Treating credential-capable URLs as `Secret<T>` is a
  target invariant, not a claim about current parsers.
- I did not prove that the catalogue sees dynamically assembled names. It
  explicitly says it does not (`docs/reference/env-vars.md:12-19`); the raw-read
  source gate and typed accessor are designed to close that gap.
- I did not mutation-test the proposed TOML, Compose, registry, or runtime
  checks. The negative fixtures and anti-vacuity assertions are requirements
  for implementation, not completed verification.
