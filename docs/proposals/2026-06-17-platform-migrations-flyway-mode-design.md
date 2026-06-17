# Platform Migrations on `zeroship-migrate` — Flyway-style Platform Profile

Status: **proposal** (pre-implementation; security-first). Date: 2026-06-17.
Scope: replace **Liquibase** (the platform's own DB migration tooling for the
`zeroship` / `oauth_hydra` schemas) with our own `zeroship-migrate` engine, by
adding a second **trust profile** ("Platform") and a native **Flyway-style
file-based loader** + a `zeroship-migrate` CLI binary. This reverses the
"platform stays on Liquibase" rule in the 2026-06-16 engine design (§1.7) — see
§3.

> Authoring note (per `feedback_proposal_workflow`): this draft is **uncommitted**
> until the implementing PR; written here for the design-critic loop. Decisions in
> §4–§6 are **LOCKED** (decided) — this doc states them as the design with
> rationale + security analysis, not as open questions.
>
> **Round 2 revision** (addressing the adversarial design-critic): the §5 call-site
> invariant is now made *statically true* (private `GuardConfig` fields +
> `#[non_exhaustive]` crate-private `Platform`, §4.1/C1); the body-scan denial layer
> widening is specified (§4.2/C2); the flip list is replaced by a complete,
> code-verified table incl. the `.down.sql`-only constructs (§4.1/C3); the CLI
> honours the destructive gate (§9/H1); role-password secrets are acknowledged
> (§8.1/H2); the Confined-unchanged proof enumerates every schema read site
> (§6.4/H3); plus M1–M4, the platform advisory-lock sentinel, and L1–L3. A gap the
> draft missed — the executor re-deriving its own Confined guard
> (`executor.rs:890/1083/2558`) — is closed by threading the profile through
> `ExecutorConfig` (§4.1, R6).
>
> **Round 3 revision** (addressing the focused re-critic; the EXTERNAL trust
> boundary and the §4.1 flip table were confirmed correct and are LEFT UNTOUCHED):
> (HIGH-1) the *in-crate* "by construction" overclaim is fixed — `platform()` now
> requires a `PlatformCapability` token constructible ONLY inside a private
> `platform_runner` submodule, so even in-crate `submit`/`engine` cannot mint
> Platform; the doc now claims "by construction" only where true (external: always;
> in-crate: with the token). (HIGH-2) a FOURTH executor-path guard —
> `precondition::evaluate` (`precondition.rs:552`, reached from `executor.rs:992`
> versioned + `:1112` repeatables via `evaluate_preconditions` `:1234`) — is now
> threaded trust-aware (§4.1, §6.4 now 8 sites). (HIGH-3) the §4.2 foreign-schema
> body relaxation is corrected: those helpers scan only `PLATFORM_SCHEMAS =
> ["control","auth","billing"]`, NOT the port schemas, so relaxing it is a no-op for
> the 56-file port; the relaxation is reframed as a future-platform-body need behind
> the operator allowlist. The TOKEN-scan needle relaxation stays (it IS needed).
> Plus MINOR-1 (14 construction sites, not 11), MINOR-2 (submit.rs literal is
> `:419`, not `:248`), MINOR-3 (the GRANT flip-table objtype column is descriptive,
> not a per-objtype gate).

---

## 1. Goal

One engine — `zeroship-migrate` — runs **both** the untrusted creator-project
migrations (today's "Confined" profile) and the **trusted platform schema**
migrations (the new "Platform" profile), with the trust posture set **at the
operator call site, never derived from migration content**. Concretely:

1. Add a **Platform** trust profile to the engine: trusted operator SQL, a
   **widened** guard that permits the privileged DDL the platform schema needs
   (`CREATE ROLE`/`GRANT`/`REVOKE`/RLS/`CREATE SCHEMA` + the platform schemas + a
   curated extension allowlist) **while keeping the RCE/host-escape backstop on**.
2. Add a native **Flyway-style file loader** (`V<NNNN>__<desc>.sql` /
   `.down.sql` / `R__<desc>.sql`) — *not* a Liquibase-header parser.
3. Ship a `zeroship-migrate` **CLI** (`migrate` / `status` / `validate` /
   `rollback`) that replaces the Liquibase `migrate` compose service and
   `ops/db-migrate.sh`.
4. Port the 56 changeset files to the file format, swap the compose service, and
   **delete** the Liquibase changelog. Fresh DBs re-migrate from scratch (no
   `DATABASECHANGELOG` adoption — §7).

Non-goal: this does **not** touch the creator-project (Confined) path, which is
already built and stays bit-for-bit unchanged (the confined-default-unchanged
proof is §6.4). It also does not unify the *roles*: a Platform migration still
runs as the admin connection (§8), with the creator `migrator` role untouched.

---

## 2. Background — ground truth

State, not re-derivation (from a completed mapping of the live tree):

- **56 changeset files** `db/changelog/changesets/0001…0057_*.sql` (gap at
  `0045`), holding **168 Liquibase changesets** total. Raw "`--liquibase
  formatted sql`", ordered by filename via `includeAll` in
  `db/changelog/db.changelog-master.yaml:18`.
- **Pervasive `--rollback` blocks**, one per changeset (e.g.
  `0001_extensions_schemas.sql:13`, `0025_roles_rls.sql:241,255`).
- **Schemas touched:** `zeroship` (the single platform schema —
  auth/control/billing/**sandbox** are *table-name groups inside it*, not separate
  schemas; the historical `sandbox` schema was folded into `zeroship` at
  consolidation, `0011_sandbox_initial.sql:11`), `oauth_hydra`
  (`0027_oauth_hydra_schema.sql:31`), and `public` (Liquibase tracking tables
  `DATABASECHANGELOG`/`DATABASECHANGELOGLOCK` only — `ops/db-migrate.sh:44–45,53`).
- **Extensions:** `citext` (`0001:12`, into `zeroship`/default) and `uuid-ossp`
  (`0027:48`, `WITH SCHEMA oauth_hydra`, pre-created as superuser for Hydra).
- **~45 of 56 files would be HARD-DENIED by the creator (Confined) guard:**
  `CREATE ROLE` / `GRANT` / `REVOKE` (`0025`, `0027`, `0032`), `CREATE EXTENSION`
  (`0001`, `0027`), `CREATE SCHEMA` (`0001`, `0027`), RLS `CREATE POLICY` + `FORCE
  ROW LEVEL SECURITY` (`0025:250–293`), and cross-schema references
  (`oauth_hydra`, `public`). These are exactly the categories the Confined guard
  is built to deny (`guard.rs:223–248` role/grant/db/fdw, `:289–302` extensions,
  `:373` deny-by-default for `CREATE POLICY`/`CreatePolicyStmt`).
- **2 `validCheckSum ANY` changesets** (`0025_roles_rls.sql:84`,
  `0031_app_members_owner_backfill.sql:45`) — Liquibase escape hatches that let an
  already-applied changeset's body change without a checksum failure.
- **The `migrate` compose service** (`docker-compose.yml:81–95`) runs the
  `liquibase/liquibase:4.31` image with `update` once at boot as `postgres`;
  `control`/`auth` gate on `service_completed_successfully`
  (`docker-compose.yml:164–165`). **Hydra migrates its OWN tables separately**
  (`hydra migrate`, the `hydra-migrate` service) into `oauth_hydra` as the
  `oauth_hydra` role — **not** in this changelog (`0027:10–14`).
- **`ops/db-migrate.sh`** is a dev wrapper (`status` / `update` / `update-sql` /
  `validate` / `rollback-count` / `changelog-sync`) shelling into the Liquibase
  image.

**Engine today** (`crates/zeroship-migrate/`):

- Versioning is **UUIDv7-only** (`MigrationId` = `mig_<base62 uuidv7>`,
  `migration.rs:30–85`); ordering is by version + `depends_on`
  (`executor.rs:9`, `lib.rs` re-exports).
- **No file/dir loader** — migrations arrive via `submit.rs` (one script) or the
  declarative/expand-contract authors. Library-only (**no CLI bin**).
- `GuardConfig` is `{ project_schema: String, extension_allowlist: Vec<String> }`
  (`guard.rs:36–46`) — **single schema, no trust mode, no multi-schema**.
- The engine is already **compio-native** (`db.rs`, `compio_postgres::Client`),
  zero tokio.

---

## 3. The invariant reversal — and why it is safe

The 2026-06-16 engine design **§1.7** says, verbatim:

> "The creator-migration engine's roles have **zero** access to
> `control`/`auth`/`billing`. The platform's own db therefore stays on
> **Liquibase** (engineer-authored, separate trust domain). Unifying them would
> hand the creator-migration path a route toward platform schemas — security-first
> says **do not unify**."

**This design reverses the conclusion ("stays on Liquibase") while preserving the
premise ("do not hand the creator path a route to platform schemas").** The §1.7
reasoning conflated two separable things:

1. **Trust domain separation** (the creator path must never reach `control`/`auth`
   /`billing`). — *Preserved, strengthened.*
2. **Tool separation** (therefore use a *different binary*, Liquibase). — *Dropped.*

(2) was a proxy for (1). Liquibase enforced trust separation **physically**: a
different image, a different invocation, no shared code with the creator engine.
The reversal is safe because we replace *physical* separation with a **typed
in-engine invariant** (§5): the Platform profile is constructible **only** at the
operator call site (the CLI / compose `migrate` service), and the creator
submission ingress (`submit_migration`) is hard-wired to Confined with **no API
path** to Platform. The trust boundary moves from "which binary you run" to "which
*capability handle* the call site can construct" — and the latter is statically
enforced and unit-testable, where the former was a deploy-time convention. (2)
bought us nothing (1) doesn't buy more rigorously; and unifying gains a single,
audited, zero-tokio, in-stack security core for the platform schema too.

<!-- Revised in round 2: addressing L1 — scope the "strictly better than
     Liquibase" claim to exactly the two axes where it holds, not a blanket
     win. -->
**Where the engine is strictly better than Liquibase — and where it is the
same.** The win is precisely **two axes**, both *parse-time / integrity*, not
*privilege*:

1. **A parse-time deny-list backstop.** Liquibase has *none* — it ships whatever
   SQL the changeset contains to the server verbatim. `zeroship-migrate` parses
   every statement (and every DO-block / function body / EXECUTE literal,
   `guard.rs:622–724`) with the real `pg_query` parser and **hard-denies the
   RCE / host-escape / file / network surface even under Platform** (§4). That
   backstop is new and is the meaningful security gain.
2. **Per-migration checksum + manifest tamper-evidence.** `Checksum::of`
   (`migration.rs:287`) folds the whole apply-relevant unit and the engine aborts
   on drift (`executor.rs` step 4) — stronger than Liquibase's separate
   `DATABASECHANGELOG` MD5 with its `validCheckSum ANY` escape hatch.

The **privilege posture is UNCHANGED** and is *not* a win: §8 keeps the admin
connection, exactly as Liquibase runs as `postgres`. We do not claim the engine
tightens privilege; that is explicitly deferred (§8, future least-priv
platform-migrator role). Scoping the claim this way means a critic cannot point at
"but it still runs as a superuser-equivalent" as a contradiction — we say so up
front.

Net: §1.7 gets rewritten in the implementing PR from "stays on Liquibase" to
"runs on `zeroship-migrate` under the **Platform** profile; trust separation is
the §5 call-site invariant, not tool separation."

---

## 4. Architecture — two profiles, one engine

`zeroship-migrate` gains a **trust profile** as a first-class input to the guard.
The journal, advisory lock, checksum, manifest, drift check, and two-phase non-txn
recovery are **shared and unchanged**. What changes for Platform: the guard's
deny-list widens (both the top-level kind gate **and** the DO-block body scan,
§4.1/§4.2), and the profile is threaded into the executor's internal guard
(`executor.rs:890/1083/2558`) via a `pub(crate)` field on `ExecutorConfig` so a
Platform plan is not re-denied by the executor's static first-pass (§4.1).

| Aspect | **Confined** (exists today) | **Platform** (new) |
| --- | --- | --- |
| Trust of SQL author | Untrusted creator + prompt-injectable AI | Trusted operator/engineer (reviewed in-repo) |
| Set at | `submit_migration` (hard-wired) | CLI / compose `migrate` only |
| Schemas allowed | One (`project_schema`) | Multi: `zeroship`, `oauth_hydra`, `public` (allowlist) |
| Privileged-kind set (role / grant / schema / RLS / policy) — **the exact list is §4.1** | **DENY** | **ALLOW iff Platform** (§4.1 flip table) |
| Cross-schema references | DENY (`:395–403`) | **ALLOW** (within the schema allowlist only) |
| `CREATE EXTENSION` | allowlist-gated, default empty | allowlist incl. `citext`, `uuid-ossp` |
| `COPY … PROGRAM` (shell RCE) | DENY (`:213`) | **DENY** (kept) |
| `COPY … <file>` (filesystem) | DENY (`:216`) | **DENY** (kept) |
| Untrusted PLs (`plpythonu`/`plperlu`/`c`/`plv8`) | DENY (`:258–261`) | **DENY** (kept) |
| `dblink` / `*_fdw` (FDW + SSRF) | DENY (`:240–246`, `FORBIDDEN_EXTENSIONS`) | **DENY** (kept) |
| File/network funcs (`pg_read_file`, `dblink_*`) | DENY (`:464–481`) | **DENY** (kept) |
| `ALTER SYSTEM` | DENY (`:223`) | **DENY** (kept) |
| `LOAD <library>` | DENY (`:248`) | **DENY** (kept) |
| `SECURITY DEFINER` functions | DENY (`:265`) | **DENY** (kept — see §6.3) |
| `COPY … PROGRAM` / file / network / untrusted-PL **hidden in bodies** | DENY (body scan, `:635–696`) | **DENY** (kept) |
| `create role`/`drop role`/`search_path` **substring in a body** | DENY (body token-scan, `:678–685`) | **ALLOW iff Platform** (§4.2) |
| Foreign-schema **inside a body** (`foreign_schema_in_body` `:701`/`foreign_schema_literal_in_body` `:716`) | DENY iff schema ∈ `PLATFORM_SCHEMAS` = `{control,auth,billing}` only | **UNCHANGED for the port** (port schemas `zeroship`/`oauth_hydra`/`public` ∉ `PLATFORM_SCHEMAS`, so already pass — §4.2/HIGH-3); future: `allowlist ∪ PLATFORM_SCHEMAS` |
| Runs as | admin `SET ROLE migrator_<project>` (NOSUPERUSER) | admin connection directly (§8) |
| Versioning | UUIDv7 (`mig_…`) | File version `V<NNNN>` (§6) |
| Ordering | UUIDv7 + `depends_on` | numeric filename prefix (§6.2) |

The defining property: **Platform is "lint-mode-PLUS-RCE-backstop", not
"lint-only".** Even trusted operator SQL is still parsed by the real `pg_query`
parser and still denied the host-escape / RCE / file / network surface — defense
in depth survives, because a trusted author can still make a mistake (or a
supply-chain edit can slip in) and `COPY … PROGRAM` has no legitimate place in a
platform schema migration any more than a creator one.

### 4.1 How `GuardConfig` grows — a granted capability, NOT a settable field

<!-- Rewritten in round 2: addressing C1 (the call-site invariant was false as
     written — public fields + a public `Platform` variant let a struct literal
     forge Platform) and C3 (the flip list was incomplete and uncited).
     Round 3 (HIGH-1): the round-2 mechanism (private fields + #[non_exhaustive]
     + pub(crate) platform()) closes the EXTERNAL boundary by construction (an
     external crate cannot name Platform nor reach the constructor — that claim
     stands and is the real threat model). But it does NOT make the IN-CRATE
     story "by construction": submit.rs/engine.rs live in the SAME crate, so
     pub(crate) does not stop them calling platform(). That was reviewed
     convention, not compiler-enforced. The fix below adds a PlatformCapability
     token mintable only inside a private `platform_runner` submodule, so even
     in-crate code cannot construct Platform without the token. The doc now
     claims "by construction" precisely: external always; in-crate with the
     token. -->

`GuardConfig` gains a trust **profile** and a **schema scope**.

**What "by construction" means here — external vs. in-crate, stated precisely.**
Two boundaries, two different strengths of guarantee:

- **The EXTERNAL boundary (control / builder / any other crate — the real
  threat) is closed BY CONSTRUCTION.** `trust`/`schemas`/`extension_allowlist`
  are private and `TrustProfile` is `#[non_exhaustive]`, so an external crate can
  neither write a `GuardConfig { trust: Platform, .. }` literal nor *name*
  `TrustProfile::Platform` at all — not in a literal, a match, or as an argument.
  This is the claim that replaces Liquibase's physical separation and it is
  statically, compiler-enforced true. The §12 T8 trybuild test pins it.
- **The IN-CRATE story was convention, NOT "by construction" — round 3 closes it
  with a capability token.** All engine modules (`submit`, `engine`, `executor`,
  `shadow`, …) share one crate, so `pub(crate) fn platform()` does *not* stop
  `submit.rs`/`engine.rs` from calling `platform()` in-crate; round 2 overstated
  that as "statically true". The honest mechanism is a **zero-sized
  `PlatformCapability` token whose constructor is private to a `platform_runner`
  submodule** (the only module that backs the CLI / compose `migrate` service).
  `GuardConfig::platform()` and `ExecutorConfig::platform()` *require the token as
  an argument*. So even in-crate, `submit`/`engine` cannot mint Platform: they
  have no way to obtain a `PlatformCapability`. A `Platform` guard is thus a
  *capability handle you are granted by holding the token*, not a *field you set
  on a struct literal* and not merely a *constructor you remembered not to call*.

```rust
/// The trust posture of a guard. Set at the OPERATOR CALL SITE, never derived
/// from SQL content. NON-publicly-constructible for `Platform`.
///
/// `#[non_exhaustive]` forbids an external crate from naming ANY variant in a
/// struct/enum literal or exhaustive match — so even `Confined` can only be
/// obtained via a constructor, and `Platform` cannot be written down at all
/// outside this crate (the EXTERNAL boundary, closed by construction). Within
/// the crate, `Platform` is produced ONLY inside `GuardConfig::platform(...)`,
/// which now REQUIRES a `PlatformCapability` token (below) — so in-crate code
/// (`submit`/`engine`) cannot mint it either without holding the token (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrustProfile {
    /// Untrusted creator/AI SQL. The full deny-list (today's behaviour).
    Confined,
    /// Trusted operator SQL for the platform schemas. Constructed ONLY by
    /// `GuardConfig::platform`, which requires a `PlatformCapability` token.
    Platform,
}

/// ROUND 3 (HIGH-1): the in-crate enforcement primitive. A zero-sized capability
/// token whose ONLY constructor (`new`) is private to the `platform_runner`
/// submodule. `GuardConfig::platform` / `ExecutorConfig::platform` take `&self`-
/// agnostic `PlatformCapability`, so the ability to produce `Platform` is gated on
/// *holding a token you can only get inside `platform_runner`* — not on a
/// pub(crate) function any in-crate module could call. This is what upgrades the
/// in-crate story from "reviewed convention" to "by construction".
mod platform_runner {
    /// Re-exported (as a TYPE only) from the crate; its `new()` is NOT.
    pub struct PlatformCapability(());

    impl PlatformCapability {
        /// The single mint site, private to this submodule. The CLI / compose
        /// `migrate` entrypoint lives in THIS module, so only it can call `new`.
        pub(super) fn new() -> Self { PlatformCapability(()) }
        // ... the CLI `migrate`/`status`/`validate`/`rollback` entrypoints (§9)
        //     live here and are the only callers of `PlatformCapability::new()`,
        //     `GuardConfig::platform(..)`, and `ExecutorConfig::platform(..)`.
    }
}
// `PlatformCapability` the TYPE is visible crate-wide (so `platform()` can name it
// in its signature) but `PlatformCapability::new` is `pub(super)` — unreachable
// from `submit`/`engine`/`executor`/etc. The §12 T11 unit pins that ONLY
// `platform_runner` can construct it.
pub(crate) use platform_runner::PlatformCapability;

pub struct GuardConfig {
    /// PRIVATE. The trust posture. Settable only through `confined()`/`platform()`.
    trust: TrustProfile,
    /// PRIVATE. The schemas this guard permits references to. Confined ⇒
    /// `Single(project_schema)`; Platform ⇒ `Allowlist([...])`.
    schemas: SchemaScope,
    /// PRIVATE. CREATE EXTENSION allowlist (FORBIDDEN_EXTENSIONS still override it
    /// in BOTH profiles). Private so the only way to obtain a non-empty allowlist
    /// is via `platform()` — a Confined config always has an empty one, exactly
    /// as `submit.rs` builds today (`extension_allowlist: Vec::new()`).
    extension_allowlist: Vec<String>,
}

impl GuardConfig {
    /// The ONLY constructor reachable from the submission ingress and every
    /// creator-path author. Always `Confined`, single-schema, empty extensions.
    /// Needs NO token — Confined is the safe default anyone may construct.
    pub fn confined(project_schema: impl Into<String>) -> Self {
        Self { trust: TrustProfile::Confined,
               schemas: SchemaScope::Single(project_schema.into()),
               extension_allowlist: Vec::new() }
    }

    /// Platform profile. REQUIRES a `PlatformCapability` token (mintable only in
    /// `platform_runner`), so neither an external crate (cannot name `Platform`
    /// nor construct the token) NOR an in-crate module (cannot construct the
    /// token) can produce a Platform guard outside the operator runner. The
    /// `_cap` arg is the in-crate enforcement; `#[non_exhaustive]` is the external
    /// enforcement. This is the single place `TrustProfile::Platform` is named.
    pub(crate) fn platform(
        _cap: &PlatformCapability,
        schemas: Vec<String>,
        extension_allowlist: Vec<String>,
    ) -> Self {
        Self { trust: TrustProfile::Platform,
               schemas: SchemaScope::Allowlist(schemas),
               extension_allowlist }
    }

    /// Read accessor the guard internals use (was a bare field read before).
    pub(crate) fn trust(&self) -> TrustProfile { self.trust }
}

impl Default for GuardConfig {
    /// Confined, empty single-schema, empty extensions — today's behaviour.
    fn default() -> Self { Self::confined(String::new()) }
}
```

`SchemaScope` replaces the bare `project_schema: String`:

- `SchemaScope::Single(String)` — the **Confined** shape. The existing
  `check_cross_schema` / `foreign_schema_in_tree` / `check_func_def_target` /
  `check_literal_schema_refs` / body checks consult it *exactly* as they consult
  `project_schema` today (one allowed schema; everything else `CrossSchema`).
- `SchemaScope::Allowlist(Vec<String>)` — the **Platform** shape: a reference is a
  `CrossSchema` violation **only if its schema is not in the allowlist** — so
  `oauth_hydra` ↔ `public` ↔ `zeroship` passes, but a reference to a *creator
  project schema* (`proj_…`) still fails closed. §6.4 proves every read site
  projects `Single` to byte-identical behaviour.

<!-- Round 3 (MINOR-1): the prose said "11 sites" but the code has 14 literal
     `GuardConfig{…}` constructions; the "compiler forces completeness" argument
     depends on the count being exact, so it is corrected to 14. (MINOR-2): the
     submit.rs literal is at :419, not :248. -->
**Blast radius of making the fields private.** This is larger than the round-1
draft implied: `GuardConfig` is built by a **struct literal in 14 sites** today
(submit ×1, executor ×3, engine-test ×1, shadow ×4, baseline ×1, author ×1,
backfill ×1, precondition ×1, squash ×1) — `submit.rs:419`,
`executor.rs:890` / `:1083` / `:2558`, `engine.rs:963` (test), `shadow.rs:542` /
`:740` / `:783` / `:936`, `baseline.rs:124`, `author.rs:383`, `backfill.rs:747`,
`precondition.rs:552`, `squash.rs:165`. **Every one moves to
`GuardConfig::confined(<project_schema>)`** in the same PR (all are
creator-path/Confined; none should ever be Platform). Privatising the fields is
what *forces* this — a literal `GuardConfig { … }` stops compiling crate-wide, so
the compiler enumerates all 14 for us (the count must be exact for that
"compiler-forces-completeness" argument to hold). The operator-side Platform
config is born only inside the new `platform_runner` via `GuardConfig::platform(
&cap, …)`, the single Platform-minting site.

#### The privileged-kind flip table (the builder's exact spec — C3)

Under `self.cfg.trust() == Platform`, the following constructs move from the
hard-deny arm of `check_statement_kind` (`guard.rs:207–376`) to an
**allow-iff-Platform** arm. Every construct was verified against `guard.rs` and the
56 ported changesets. **The "for `.down.sql`" column marks constructs needed ONLY
by the reverse files** — these were *missing* from the round-1 list and would have
made every rollback fail; they are the second half of the spec.

| Construct (SQL) | `pg_query` node / discriminant | Confined | Platform | Up? | `.down.sql`? | Used by |
| --- | --- | --- | --- | --- | --- | --- |
| `GRANT …` / `REVOKE …` | `GrantStmt` (`is_grant` true/false; objtype column is DESCRIPTIVE — see note) | DENY `:230` | ALLOW | ✓ | ✓ (REVOKE in down) | 0025 grants, 0027:32–33 `GRANT CONNECT ON DATABASE`/`USAGE ON SCHEMA` |
| `GRANT role` / role membership | `GrantRoleStmt` | DENY `:231` | ALLOW | ✓ | ✓ | (none today; allowed for symmetry) |
| `ALTER DEFAULT PRIVILEGES` | `AlterDefaultPrivilegesStmt` | DENY `:232` | ALLOW | ✓ | ✓ | (none today; allowed for symmetry) |
| `CREATE ROLE` | `CreateRoleStmt` | DENY `:225` | ALLOW | ✓ | — | 0025 (EXECUTE'd), 0027:24 |
| `ALTER ROLE …` | `AlterRoleStmt` | DENY `:226` | ALLOW | ✓ | ✓ | (none today; allowed for symmetry) |
| `ALTER ROLE … SET search_path = …` (×17 across files) | `AlterRoleSetStmt` | DENY `:227` | ALLOW | ✓ | ✓ (`RESET search_path`, 0027:37) | 0025:146–150, 0027:36 |
| `DROP ROLE …` | `DropRoleStmt` **AND** `DropStmt` `remove_type == ObjectRole` | DENY `:228` **and** `:328` | ALLOW (both spellings) | — | ✓ (down only) | 0025:241, 0027:28 (`DROP ROLE IF EXISTS`) |
| `CREATE SCHEMA …` | `CreateSchemaStmt` | DENY (deny-by-default `:373`) | ALLOW | ✓ | — | 0001, 0027:31 |
| `DROP SCHEMA …` | `DropStmt` `remove_type == ObjectSchema` | DENY (`is_safe_drop_object` `:855` excludes it) | ALLOW | — | ✓ (down only) | 0027:38 `DROP SCHEMA … CASCADE` |
| `CREATE POLICY …` (RLS) | `CreatePolicyStmt` | DENY (deny-by-default `:373`) | ALLOW | ✓ | — | 0025:252/263/274/288 |
| `DROP POLICY …` | `DropStmt` `remove_type == ObjectPolicy` | DENY (`is_safe_drop_object` excludes it) | ALLOW | — | ✓ (down only) | 0025:255/266/277/291 |
| `ALTER TABLE … ENABLE ROW LEVEL SECURITY` | `AlterTableCmd` subtype `AtEnableRowSecurity` | DENY (`is_safe_alter_table_subtype` `:877` excludes it) | ALLOW | ✓ | — | 0025:250/261/272/286 |
| `… FORCE ROW LEVEL SECURITY` | `AtForceRowSecurity` | DENY (excluded) | ALLOW | ✓ | — | 0025:251/262/273/287 |
| `… NO FORCE ROW LEVEL SECURITY` | `AtNoForceRowSecurity` | DENY (excluded) | ALLOW | — | ✓ (down only) | 0025:256/267/278/292 |
| `… DISABLE ROW LEVEL SECURITY` | `AtDisableRowSecurity` | DENY (excluded) | ALLOW | — | ✓ (down only) | 0025:257/268/279/293 |
| `CREATE EXTENSION <ext>` | `CreateExtensionStmt` | allowlist-gated, default empty | allowlist = `[citext, uuid-ossp]` (FORBIDDEN_EXTENSIONS still override — §13 R4, L3) | ✓ | — | 0001 `citext`, 0027:48 `uuid-ossp` |
| `DROP EXTENSION <ext>` | `DropStmt` `remove_type == ObjectExtension` | DENY (excluded) | ALLOW | — | ✓ (down only) | 0027:49 `DROP EXTENSION IF EXISTS "uuid-ossp"` |
| `DROP OWNED BY <role>` | `DropOwnedStmt` | DENY (deny-by-default `:373`) | ALLOW | — | ✓ (down only) | 0025:241 rollback DO-block |

<!-- Round 3 (MINOR-3): clarify the GRANT row's objtype column is descriptive
     (what 0025/0027 happen to grant ON), NOT a per-objtype gate to implement. -->
> **Note on the `GRANT …` objtype column (MINOR-3).** "objtype ∈ {table,
> sequence, schema, database}" is **descriptive** — it lists what the ported
> changesets happen to `GRANT … ON` — **not a per-objtype allow gate to
> implement.** Today a `GrantStmt` is denied **wholesale** at `guard.rs:230` (the
> arm matches `GrantStmt | GrantRoleStmt | AlterDefaultPrivilegesStmt` and returns
> `denied(PRIVILEGE_MANAGEMENT)` with no objtype inspection). The flip allows the
> **whole `GrantStmt` arm** under Platform — an implementer must NOT add a spurious
> `objtype`-discriminating filter; there is no objtype branch in the current code
> and none should be introduced. (The RCE/host-escape backstop does not live in
> `GrantStmt`, so allowing the whole arm is sound.)

Mechanism: each row is implemented as a guarded early-`return Ok(())` (or an
allowlist check, for extensions) inside the matching arm — `if
self.cfg.trust() == TrustProfile::Platform { return Ok(()); }` *before* the
existing `return Err(denied(...))`. For the deny-by-default constructs
(`CreateSchemaStmt`, `CreatePolicyStmt`, `DropOwnedStmt`) a new arm is added that
allows-iff-Platform and otherwise falls through to the unchanged `_ =>` deny. For
the predicate-gated constructs (`is_safe_drop_object`, `is_safe_alter_table_subtype`)
the predicate gains a `trust`-aware overload (or the call sites gain an
`|| (trust == Platform && is_platform_drop/alter(…))` clause) so the Platform
extra set (ObjectSchema/ObjectExtension/ObjectPolicy/ObjectRole drops; the four
RLS subtypes) is admitted only under Platform. **Every other arm — all of §4's
"kept" rows — is byte-for-byte unchanged and runs in both profiles.** This is the
crux of the confined-unchanged proof (§6.4): the Confined path takes the
*identical* code it does today because every new branch is gated and unreachable
when `trust == Confined`.

#### Threading the profile to the executor's internal guard (a gap the draft missed)

<!-- New in round 2: while grounding C1/C2 against the code I found the
     executor re-derives its OWN guard from ExecutorConfig.project_schema at
     executor.rs:890/1083/2558 — so a Platform PLAN would still be DENIED by the
     executor's static first-pass. The profile MUST reach the executor too.
     Round 3 (HIGH-2): there is a FOURTH executor-path guard the round-2 list
     missed — `precondition::evaluate` builds its OWN hardcoded Confined
     GuardConfig (precondition.rs:552-555) and is invoked from BOTH the versioned
     apply loop (executor.rs:992) and the repeatables loop (:1112) via
     evaluate_preconditions (:1234). It must be threaded trust-aware too. -->
`engine.plan(set, &GuardConfig)` is not the only guard in the apply path. The
executor builds **other** `SqlGuard`s that today each re-derive a Confined config
from `ExecutorConfig.project_schema`, so a Platform plan would pass `engine.plan`
and then be **re-denied** by them. There are **four** such sites:

1. The static first-pass for versioned migrations (`executor.rs:890`).
2. The static first-pass for repeatables (`executor.rs:1083`).
3. The `rollback` guard (`executor.rs:2558`).
4. **The precondition guard (HIGH-2).** `precondition::evaluate`
   (`precondition.rs:305`) → `evaluate_sql_boolean` builds a hardcoded
   **`GuardConfig { project_schema, extension_allowlist: Vec::new() }`**
   (`precondition.rs:552-555`) to guard a `SqlBoolean` precondition before
   running it. It is reached from BOTH the versioned apply loop
   (`executor.rs:992`) and the repeatables loop (`:1112`), via
   `evaluate_preconditions` (`:1234`). This is a fourth executor-path guard, not
   just the three named in round 2.

So the trust profile must reach the executor (and the precondition evaluator) as
well, *without* becoming a forgeable request field:

- **`ExecutorConfig` gains a `pub(crate)` `trust: TrustProfile` field**, defaulting
  to `Confined` via the existing `ExecutorConfig::new(...)` (`db.rs:58`), and set
  to `Platform` ONLY by a new `pub(crate)` `ExecutorConfig::platform(&cap, ...)`
  constructor that — like `GuardConfig::platform` — **requires the
  `PlatformCapability` token** (HIGH-1) and is called only from `platform_runner`.
  Because the field is `pub(crate)`, the only `Platform`-producing constructor
  requires a token no other module can mint, and the enum is `#[non_exhaustive]`,
  `submit_migration` (which receives `&ExecutorConfig` from the control plane)
  cannot flip it — and the control plane, being outside the crate, can never name
  `Platform` either (§5 / §12 T8 compile-fail test).
- The executor's **four** internal guard sites — the three static-first-pass /
  rollback guards (`890`/`1083`/`2558`) **and `precondition::evaluate`'s guard
  build (`precondition.rs:552-555`, reached from `executor.rs:992`/`:1112`)** —
  build `GuardConfig::confined(...)` when `cfg.trust == Confined` and
  `GuardConfig::platform(&cap, self.platform_schemas, self.platform_exts)` when
  `Platform`. The platform schema allowlist + extension allowlist (and the
  `PlatformCapability` the runner threads through) ride on the same
  `pub(crate)`-constructed `ExecutorConfig`, so they too are operator-supplied,
  never request-derived.
- **The precondition site, specifically (HIGH-2).** `evaluate_sql_boolean`'s
  hardcoded `GuardConfig { project_schema, extension_allowlist: Vec::new() }`
  becomes trust-aware: it reads `cfg.trust` off the `&ExecutorConfig` it already
  takes and builds `confined(cfg.project_schema)` or `platform(&cap,
  cfg.platform_schemas, cfg.platform_exts)` exactly like the other three. It is
  **latent for the 56-file port** — the loader sets `preconditions = []` (§6.3),
  so no ported file carries a `SqlBoolean` precondition today — but it MUST be
  threaded anyway: a *future* platform migration whose precondition references a
  platform schema (e.g. `SELECT EXISTS(SELECT 1 FROM oauth_hydra.clients)`) would
  otherwise be wrongly `Denied` by the unconditionally-Confined precondition
  guard, even running under Platform. Leaving it hardcoded would be a silent
  Platform-mode hole the day a platform precondition is written.

Net: the capability handle is **one profile, threaded through the planner's
`GuardConfig`, the executor's `ExecutorConfig` (its three internal guards), and
the precondition evaluator** — all four executor-path guards — every one
non-forgeable from the creator ingress and gated on the `PlatformCapability`
token in-crate. §5 states the full enforcement; §6.4 proves Confined is unchanged
at every site.

### 4.2 The body-scan denial layer must widen too (C2)

<!-- New in round 2: addressing C2 — the round-1 draft widened only
     check_statement_kind. The body layer (check_body_text, guard.rs:635–724)
     independently HARD-DENIES create role/drop role/search_path substrings and
     foreign-schema-in-body UNCONDITIONALLY, so it would deny 0025's DO-block
     (CREATE ROLE/ALTER ROLE…search_path via EXECUTE literals) and 0027's DO $$
     CREATE ROLE even under Platform. The widening MUST extend into the body. -->

0025 and 0027 hide their privileged DDL **inside DO blocks** — 0025's
`DO $bootstrap$` `EXECUTE 'CREATE ROLE …'` / `EXECUTE 'ALTER ROLE … SET
search_path …'` / `EXECUTE 'GRANT …'`, and 0027's `DO $$ … CREATE ROLE
oauth_hydra …'`. These never reach `check_statement_kind` as top-level nodes; they
reach `check_body_text` (`guard.rs:635–724`). That function has **two distinct
denial mechanisms with different reachability**, and the round-1 draft widened
neither:

1. **The recursion arm — already trust-correct.** `check_bodies`
   (`:622`) extracts each body string and `check_body_text` (a) re-parses it and
   recurses via `self.check_node(...)` (`:646`), and (b) extracts embedded string
   literals (`EXECUTE 'CREATE ROLE …'`) and recurses via `self.check_node(...)`
   (`:658`). **`check_node` re-enters `check_statement_kind` and `check_cross_schema`
   with the same `self.cfg`** — so once the §4.1 flip is in, a recursed
   `CreateRoleStmt` / `GrantStmt` / `AlterRoleSetStmt` / `CreatePolicyStmt`
   surfaced from a body **is already allowed under Platform with zero extra code**.
   0027's `DO $$ … CREATE ROLE …` parses cleanly and is admitted this way.
   *Verified: the recursion propagates `self.cfg`, so the trust flag carries in.*
2. **The literal token-scan arm — INDEPENDENT and must be separately
   conditionalized.** After the recursion, `check_body_text` runs a lexical
   backstop (`:664–696`) that does NOT consult the parse tree and is therefore
   **unconditional today**:
   - `:678` — `for needle in ["alter system", "create role", "create user",
     "drop role"]` → hard `BODY_INSPECTION` deny on substring match.
   - `:683` — `if lower.contains("search_path")` → hard deny.
   - `:687` — `program`+`copy`; `:691–695` — untrusted PL substrings;
     `:667–676` — file/network function names.
   These fire on the **raw body text** regardless of whether the construct parsed.
   So even with §4.1 in place, 0025's DO-block dies at `:678` (`create role`
   substring) / `:683` (`search_path` substring), and 0027's at `:678`.

   **The fix (Platform-only relaxation, surgical):** under `self.cfg.trust() ==
   Platform`, drop **exactly** the `"create role"` / `"create user"` /
   `"drop role"` needles (`:678`) and the `search_path` substring check (`:683`)
   from the token-scan. **`"alter system"` STAYS** in the needle list in both
   profiles (ALTER SYSTEM has no place in any migration; §4 keeps it). The
   `program`+`copy`, untrusted-PL, and file/network-function token scans
   (`:667–676`, `:687`, `:691–695`) **STAY HARD in both profiles** — these are the
   RCE/host-escape backstop and must survive a trusted-author mistake or
   supply-chain edit.
<!-- Round 3 (HIGH-3): the round-2 point 3 rested on a FALSE premise. The body
     foreign-schema helpers scan ONLY against PLATFORM_SCHEMAS =
     ["control","auth","billing"] (denylist.rs:249) — NOT zeroship / oauth_hydra /
     public. So the three PORT schemas already pass this body layer today;
     relaxing it for them is a NO-OP. The names this layer blocks (control / auth
     / billing) are only referenced by a FUTURE platform body, and only need
     relaxing IF the operator allowlist contains them. The TOKEN-scan relaxation
     (point 2) IS still needed — keep it. -->
3. **The foreign-schema-in-body checks — the relaxation is UNNECESSARY for the
   56-file port, and is reframed as a future-platform-body provision.**
   `foreign_schema_in_body` (`guard.rs:795`, called at `:701`; lexical
   `schema.object` scan) and `foreign_schema_literal_in_body` (`guard.rs:738`,
   called at `:716`; bare-literal `%I`-template scan) do **NOT** deny *any*
   foreign-schema reference — they scan **only against `PLATFORM_SCHEMAS =
   ["control","auth","billing"]`** (`denylist.rs:249`). Three consequences the
   round-2 draft got wrong:

   - **The 56-file port references `zeroship` / `oauth_hydra` / `public` — none of
     which is in `PLATFORM_SCHEMAS` — so the ported bodies ALREADY PASS this layer
     today.** Relaxing the body foreign-schema scan for the port schemas is a
     **no-op**: there is nothing to relax for them. (The *structural*
     `RangeVar`/cross-schema check at the top level still must admit them — that is
     the §4.1 `SchemaScope::Allowlist` work — but the *body lexical* layer never
     blocked them in the first place.)
   - **The names this body layer actually blocks are `control` / `auth` /
     `billing`** — which would only be referenced by a *future* platform-body
     migration (none of the 56 reference them; they are table-name groups inside
     `zeroship`, not schemas — §2). So a relaxation is only ever needed for a
     future platform migration whose DO-block body references `control.` / `auth.`
     / `billing.` as a *schema-qualified name*.
   - **IF that future need arises**, the body scan under Platform must consult
     **`allowlist ∪ PLATFORM_SCHEMAS`** (treat a schema as permitted iff it is in
     the operator-supplied `SchemaScope::Allowlist`, OR — for the lexical layer —
     not in `PLATFORM_SCHEMAS` as today), **and the operator must explicitly add
     `control`/`auth`/`billing` to the `--schema` allowlist.** Implementation when
     that day comes: both helpers gain `&SchemaScope` and, under `Allowlist`, treat
     "schema ∈ allowlist" as permitted while keeping the `PLATFORM_SCHEMAS` lexical
     backstop for any schema NOT in the allowlist. Under `Single` (Confined) they
     behave **exactly as today** — unchanged `PLATFORM_SCHEMAS`-scoped scan
     excluding the project schema (§6.4 site 4/5: "`Single(s)` ⇒ unchanged
     `PLATFORM_SCHEMAS` scan", confirmed correct by the re-critic).

   **For this PR, the only body-layer change is point 2's TOKEN-scan needle
   relaxation** (`create role` / `drop role` / `search_path`), which IS needed for
   0025's DO-block `EXECUTE`-literals; the foreign-schema-body helpers are left
   functionally as-is for the port (a `&SchemaScope` signature change is fine for
   §6.4 uniformity, but the *behaviour* on the port schemas is unchanged because
   they were never in `PLATFORM_SCHEMAS`).

Crucially, the deny token-scans that the Platform body **keeps** (`alter system`,
`program`+`copy`, untrusted-PL, file/network funcs) are the same RCE/host-escape
class §4's table marks "kept" — so a `DO $$ … COPY x FROM PROGRAM 'curl …' $$`
authored even by a trusted operator is still denied in Platform. The widening is
the *privilege* surface only; the *RCE/host-escape* surface is untouched.

---

## 5. THE security invariant — Platform is unreachable from the creator path

> **The Platform profile is constructible ONLY by the operator-side runner. The
> creator submission path has NO way to select it.**

This is the invariant that *replaces* Liquibase's physical separation, and the #1
thing a critic should attack. There are **two boundaries**, enforced by two
mechanisms, and the doc is now precise about which is "by construction" where
(HIGH-1):

- **EXTERNAL boundary (control / builder / any other crate — the real threat):
  closed BY CONSTRUCTION, no caveat.** `TrustProfile` is `#[non_exhaustive]` and
  the fields are private, so an external crate cannot name `Platform` nor write a
  `GuardConfig{…}` literal — full stop. This is the claim the re-critic confirmed
  airtight; it is left untouched and is enforced by §12 T8 (trybuild).
- **IN-CRATE boundary (`submit`/`engine`/`executor`/… all share the crate): round
  2 OVERSTATED this as "by construction."** `pub(crate) fn platform()` does NOT
  stop a sibling in-crate module from calling it — same-crate visibility makes it
  reachable. Round 2's "statically true" claim was only true *externally*;
  in-crate it was reviewed convention. Round 3 closes it with the
  **`PlatformCapability` token** (§4.1): `platform()` now takes `&PlatformCapability`,
  and that token's constructor is private to `platform_runner`, so even in-crate
  `submit`/`engine` cannot mint Platform — they cannot obtain the token. *Now* the
  in-crate boundary is also by construction.

The corrected enforcement, point by point:

1. **Platform is a granted capability, not a settable field — externally
   un-nameable, in-crate un-mintable.** `GuardConfig`'s
   `trust` / `schemas` / `extension_allowlist` fields are **private** (§4.1), so a
   struct literal `GuardConfig { … }` does not compile outside the module — the
   only ways to get a `GuardConfig` are `confined()` (always `Confined`) and
   `platform(&cap, …)`. Externally, `TrustProfile` is `#[non_exhaustive]`, so an
   external crate **cannot name `TrustProfile::Platform`** at all — not in a
   literal, a match, or as a function argument. In-crate, `platform()` **requires a
   `PlatformCapability`** whose only constructor is `pub(super)`-private to
   `platform_runner`, so no other in-crate module can call `platform()` either. Two
   tests pin this: §12 T8 (trybuild) for the external boundary, §12 T11 (in-crate
   unit) asserting only `platform_runner` can construct the capability.

2. **`GuardConfig::platform(&cap, …)` is `pub(crate)`, takes the token, and the
   token mint + the constructor call both live in `platform_runner`** (the code that
   backs the CLI and the compose `migrate` service). It is **not** in `lib.rs`'s
   public/submission surface (`lib.rs:119` re-exports `GuardConfig` the type, and
   `PlatformCapability` the *type* for signatures, but neither `platform()` nor
   `PlatformCapability::new`). `platform_runner` is the single site where both a
   `PlatformCapability` and a `TrustProfile::Platform` are ever produced.

3. **`submit_migration` builds its OWN Confined config and cannot be handed a
   Platform one.** Today (`submit.rs:419–420`) it constructs a struct literal:
   ```rust
   let guard_cfg = GuardConfig { project_schema: cfg.project_schema.clone(),
                                 extension_allowlist: Vec::new() };
   ```
   Under this design that literal **stops compiling** (private fields) and is
   replaced by `GuardConfig::confined(cfg.project_schema.clone())` — a constructor
   that needs no token and can only produce `trust: Confined`. `submit_migration`
   has no `PlatformCapability` in scope and no way to mint one (the constructor is
   private to `platform_runner`), so even if it *wanted* to call `platform()` it
   could not. `Submission` (the client-facing input struct, `submit.rs:88–108`)
   carries **no profile / trust / schema field**, exactly as it carries no
   `destructive`/`requires_approval` field today (and for the same reason — those
   are server judgements). There is no parameter, env var, or `Submission` field a
   client can set to widen the guard. The engine-internal literals
   (`executor.rs:890` / `:1083` / `:2558`, `precondition.rs:552`, and the other
   Confined sites — **14 total**, §4.1) likewise all move to
   `GuardConfig::confined(...)`.

4. **`ExecutorConfig`'s trust field is `pub(crate)`, Confined-by-default, and
   `Platform` requires the same token.**
   The executor needs the profile too (§4.1, "Threading the profile"), so
   `ExecutorConfig` gains a `pub(crate) trust: TrustProfile` that defaults to
   `Confined` via `ExecutorConfig::new(...)` and is set to `Platform` ONLY by
   `pub(crate) ExecutorConfig::platform(&cap, …)` — which, like `GuardConfig::platform`,
   **requires the `PlatformCapability`** and is therefore callable only from
   `platform_runner`. `submit_migration` receives `&ExecutorConfig` from the control
   plane (outside the crate), which can neither name `Platform` (non-exhaustive
   enum), nor mint the token (private constructor), nor reach the `pub(crate)`
   constructor — so it cannot flip the executor into Platform (nor, via it, the
   precondition guard — HIGH-2) any more than it can flip the planner's
   `GuardConfig`.

5. **The Platform schema + extension allowlists are operator-supplied, not
   request-derived.** They are constants baked into the `platform` runner (or
   `--schema` / extension CLI flags, §9), never read from a migration file or a
   network request.

**Why this is stronger than "different binary."** Liquibase's separation was a
deploy convention: nothing in *code* stopped someone wiring the creator path to
shell out to Liquibase-as-superuser. Here, the strongest thing a compromised
submission path can do is call `engine.plan(set, confined_cfg)` — and the guard
in Confined mode denies every privileged construct (§4) *and* the executor still
`SET ROLE`s to the NOSUPERUSER `migrator_<project>` role (`role.rs`,
`db.rs:44–49`), so even a hypothetical guard bypass dies at the DB privilege
layer. The Platform profile removes the *parse-time* widening only; it never
touches the creator role model. The two defenses are orthogonal and both intact.

Regression tests (§12) assert this directly: a **trybuild compile-fail test**
(T8, L2) proves `GuardConfig { trust: Platform, .. }` and naming
`TrustProfile::Platform` do **not** compile outside the crate (and not from
`submit`'s module); and a runtime test (T3) confirms a privileged statement
submitted through `submit_migration` is `Denied`.

---

## 6. The Flyway-style file format, versioning, and loader

### 6.1 Filename grammar (native; NOT a Liquibase-header parser)

A migration **directory** holds plain `.sql` files. The filename encodes
everything; there is no `--changeset`/`--rollback` header parsing.

```
V<NNNN>__<description>.sql           # versioned "up" migration
V<NNNN>__<description>.down.sql      # OPTIONAL reverse for the same version
R__<description>.sql                 # repeatable (re-applies on checksum change)
```

Grammar (EBNF-ish):

```
versioned    = "V" , version , "__" , description , ".sql" ;
versioned_dn = "V" , version , "__" , description , ".down.sql" ;
repeatable   = "R" , "__" , description , ".sql" ;
version      = digit , { digit } ;          (* numeric, e.g. 0001, 42, 10000 *)
description  = ident_char , { ident_char } ; (* [A-Za-z0-9_]+, underscores for spaces *)
```

- **One file = one migration.** The whole file is the migration's `up` (or
  `down`), multi-statement. This is **better than Liquibase's per-changeset
  commits**: an `up` runs **whole-file txn-atomic** (`BEGIN; <all statements>;
  INSERT journal; COMMIT`, `executor.rs:18–22`) unless it opts into the two-phase
  non-transactional path. A partially-applied file is impossible on the
  transactional path; Liquibase, by contrast, commits per changeset, so a
  multi-changeset file could half-apply.
- **No SQL comments are load-bearing.** A `--rollback` comment in a ported file
  is just a comment; the reverse lives in the sibling `.down.sql`.
- **Repeatable `R__` files** carry no version; they re-apply whenever their
  checksum changes (maps to `MigrationFlags.repeatable`, `migration.rs:156–170`).

### 6.2 Versioning — how a numeric `V<NNNN>` maps onto the engine

The engine orders by `MigrationId` (UUIDv7) today. File-based migrations need a
**deterministic filename order**. Two options:

**Option A (CHOSEN) — keep `MigrationId` UUID-only; add a separate, explicit
ordering key.** The loader parses the numeric `V<NNNN>` prefix into a
`FileVersion(u64)` and the engine orders the loaded set by `FileVersion`
ascending (then `depends_on` as a tiebreaker/refinement, reusing the existing
topo logic). The journal records the **file version string** (`"V0001"`) as the
human/identity key, alongside a deterministically-derived `MigrationId`.

<!-- Revised in round 2: addressing M1/M2 — state the LOAD-BEARING invariant
     (fixed-22-width base62 over an ascending alphabet) and pin the version→bits
     mapping as ONE canonical documented function owned by the loader, so a
     future variable-width encoder can't silently break ordering. -->
**The load-bearing invariant.** Option A's "string `Ord` == numeric order" works
**only because `uuid_to_base62` is a fixed-22-char encoding of the 128-bit UUID as
a single big-endian integer over an *ascending* alphabet** — `BASE62 =
"0123456789ABC…xyz"`, sorted so lexicographic order matches numeric order
(`crates/core/src/typed_id.rs:10–12,27–38`). Because the width is fixed (22) and
the alphabet ascends, `a < b` as 128-bit integers ⇒ `base62(a) < base62(b)`
lexicographically, which is exactly the `migration.rs:30–35` `MigrationId` `Ord`
(a `String` newtype, derived `Ord`). **If a future refactor swapped in a
variable-width or non-ascending base62 encoder, this ordering would silently
break.** The derivation function (below) and a round-trip test (§12 T9) pin the
invariant so that change fails loudly.

**The canonical, single derivation function (loader-owned).** There is exactly one
place version→id is computed:

```rust
/// Derive a deterministic, ORDER-PRESERVING MigrationId from a file version.
/// CANONICAL: this is the ONLY version→id mapping; nothing else mints platform ids.
///
/// Bit layout (128-bit UUID): the numeric version occupies the HIGH 48 bits —
/// the same six bytes UUIDv7 uses for its big-endian millisecond timestamp
/// (migration.rs:66–84) — and the low 80 bits are ZERO. So:
///   - larger version  ⇒ larger 128-bit integer
///                     ⇒ lexicographically larger 22-char base62 (invariant above)
///                     ⇒ larger MigrationId under derived Ord.
///   - determinism: same V<NNNN> ⇒ same id every load (required for re-run
///     identity + checksum-drift detection).
/// version > 2^48 is UNREACHABLE: a numeric file prefix that large is rejected by
/// the loader's `u64`-prefix parse + an explicit `version < (1<<48)` bound (we have
/// 56 files numbered ≤ 0057; the ceiling is ~2.8e14).
fn migration_id_for_version(version: u64) -> MigrationId {
    debug_assert!(version < (1u64 << 48), "file version exceeds 48-bit ordering field");
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&version.to_be_bytes()[2..8]); // high 48 bits
    // low 80 bits stay zero
    let uuid = uuid::Uuid::from_bytes(bytes);
    MigrationId::parse(&format!("mig_{}", typed_id::uuid_to_base62(&uuid)))
        .expect("derived id is a valid mig_ typed id")
}
```

This preserves the `migration.rs:30–35` invariant ("string sort yields apply
order") *and* the existing executor ordering code, with **zero changes** to
`MigrationId`, the checksum, or the journal schema — the loader is the only new
surface.

  *Trade-off:* the derived id is not a real wall-clock UUIDv7, only UUIDv7-shaped.
  That is fine: nothing in the engine reads `timestamp_ms()` as a real time for
  platform migrations; it is used purely for ordering, which the derivation
  preserves. The §12 T9 test asserts (a) determinism, (b) `MigrationId::parse`
  round-trips the derived id, and (c) `V1 < V2 < … < V10000` in both `FileVersion`
  and the derived `MigrationId::Ord`.

**Option B (alternative) — make `MigrationId` admit a non-UUID sequence form**
(`mig_v0001` or an enum `MigrationId::{ Uuid(...), Sequence(u64) }`). This is more
honest about the version being a sequence, but it ripples through `migration.rs`
(`parse`, `timestamp_ms`, the `Ord` derivation), the journal column semantics, the
typed-id contract (`^[a-z]{3}_…`), and every test — a larger, riskier change to
the security-critical migration unit for a cosmetic gain. **Rejected** in favour
of A's "loader owns the mapping, the unit is untouched."

  Net: **Option A** — the ordering key is the parsed numeric version; the journal
  records `"V<NNNN>"`; `MigrationId` is deterministically derived and unchanged in
  shape. Gaps (the missing `0045`) are irrelevant — ordering is by numeric value,
  not contiguity.

### 6.3 Checksum and how down / repeatable map to `Migration`

- **Checksum per file** is the existing `Checksum::of(ChecksumInput)`
  (`migration.rs:287`) over the whole apply-relevant unit. The loader builds a
  `Migration` from each file exactly the way `submit.rs:272–291` builds one from a
  `Submission`: `up` = file body, `down` = sibling `.down.sql` body or `None`,
  `flags` = guard-derived via `flags_for` (`guard.rs:830`) layered with
  `repeatable` from the `R__` filename, `owner_app` = a fixed platform sentinel
  (`"platform"`), `depends_on` = `[]` (file order is the dependency), then
  `Checksum::of(...)`. So a content edit to a ported file is **drift**, caught by
  the same per-migration drift check that protects creator migrations
  (`executor.rs` step 4) — stronger than Liquibase's separate `DATABASECHANGELOG`
  MD5 (§3, axis 2).
  <!-- Revised in round 2: addressing M4 — `transactional` is AUTO-DERIVED by
       running `flags_for` per file (as submit.rs does), NOT a new file marker. -->
  **`transactional` is auto-derived, not a marker.** The loader runs the guard and
  feeds the passing report to `flags_for` (`guard.rs:830`), which sets
  `transactional = !any(non_transactional statement)` from the classifier
  (`guard.rs:831`). So a ported file that contains a non-txn statement
  (`CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM`) is
  **automatically** routed to the executor's two-phase non-txn path
  (`executor.rs:472`, with its idempotency requirement) — there is **no new
  per-file `transactional` marker** (the round-1 "future marker, out of scope"
  hand-wave is removed). Platform files must stay transactional where an
  idempotent-recovery form does not exist: `CREATE POLICY` has no `IF NOT EXISTS`,
  so a policy migration cannot be non-txn — but it does not need to be (policies
  run in a transaction). **Verified: none of the 56 files is non-txn today** (no
  `CONCURRENTLY` / `ADD VALUE` / bare `VACUUM` in the changesets), so every ported
  file takes the transactional path and `flags_for` derives `transactional = true`
  for all of them.
- **`down`** → `Migration.down: Option<String>` from the `.down.sql` sibling;
  absent sibling ⇒ `None` (explicitly irreversible). `rollback` (CLI + engine,
  `engine.rs:710`) uses it, **guard-checked in the same Platform profile** —
  which is why the §4.1 flip table must include the `.down.sql`-only constructs
  (`DROP POLICY`/`DROP ROLE`/`DROP SCHEMA`/`DROP EXTENSION`/`NO FORCE RLS`/`DISABLE
  RLS`/`DROP OWNED BY`): the executor's rollback guard (`executor.rs:2558`) is
  built from the threaded Platform profile (§4.1), so without those rows every
  `.down.sql` would be `Denied` and rollback would be impossible.
- **Repeatable (`R__`)** → `MigrationFlags.repeatable = true`
  (`migration.rs:156–170`): runs after all versioned files, re-applies on checksum
  change, `down` always `None`. This is the natural home for `CREATE OR REPLACE
  VIEW/FUNCTION/TRIGGER` objects edited over time.

  Note on `SECURITY DEFINER`: it stays **hard-denied even in Platform** (§4). If a
  future platform migration genuinely needs a `SECURITY DEFINER` function, that is
  a deliberate, separately-reviewed relaxation — not folded into this profile by
  default. The 56 ported files do not use it (verified in the mapping).

### 6.4 The Confined-unchanged proof

<!-- Revised in round 2: addressing H3 — the round-1 proof only argued the kind
     gate. But `self.cfg.project_schema` is read at multiple sites, all of which
     now read `SchemaScope` instead. The proof must show each site projects
     `Single(s)` to byte-identical behaviour, and T2 must cover func-def-target +
     literal-schema-ref fixtures, not just the kind gate.
     Round 3 (HIGH-2): the executor-path guard count is EIGHT, not seven — the
     precondition guard (precondition.rs:552-555, reached from executor.rs:992/
     :1112) is the eighth schema-read site. Added as row 8. (Round 3 also fixes
     the row-5 line ref: foreign_schema_literal_in_body is called at :716.) -->

The Confined path must be **byte-for-byte the path it runs today**. Two surfaces
changed and each must be shown identical under Confined:

**(A) The statement-kind + body gates.** Every §4.1 flip and every §4.2 body
relaxation is gated `if self.cfg.trust() == TrustProfile::Platform { … }` *before*
the unchanged deny. When `trust == Confined` each gate is dead — control reaches
the *identical* `return Err(denied(...))` / deny-by-default `_ =>` arm it does
today. `confined()`/`Default` produce `trust: Confined`, `submit_migration` and the
**thirteen** other creator-path construction sites (14 total — §4.1) call only
`confined(...)`, and `Submission` has no trust field — so the Confined branch is
the only reachable one on the creator path.

**(B) Every `self.cfg.project_schema` read becomes a `SchemaScope` read.**
Privatising the field and replacing it with `SchemaScope` touches **eight** sites.
The proof obligation: under `SchemaScope::Single(s)`, each site behaves exactly as
the old `project_schema = s`. Enumerated, with the projection:

| # | Site (`guard.rs` unless noted) | Old read | `Single(s)` projection (must equal old) |
| --- | --- | --- | --- |
| 1 | `check_cross_schema` `:396` → `foreign_schema_in_tree(json, &s)` | foreign = any schema ≠ `s` | `Allowlist` ⇒ "∉ allowlist"; **`Single(s)` ⇒ "≠ s"** ✓ identical |
| 2 | `check_func_def_target` `:413,427` (funcname `parts[0]` ≠ `s` ⇒ CrossSchema) | def-target schema ≠ `s` | `Single(s)` ⇒ `parts[0] != s` — **byte-identical** ✓ |
| 3 | `check_literal_schema_refs` `:528,546` (`'control.t'::regclass`, `nextval(…)`, namespace resolvers) | literal qualifier ≠ `s` (no shared-schema exemption) | `Single(s)` ⇒ "≠ s"; `Allowlist` ⇒ "∉ allowlist" ✓ |
| 4 | `foreign_schema_in_body` `:701,795` (lexical `schema.object` vs `PLATFORM_SCHEMAS`) | platform-schema ref in body, schema ∈ `PLATFORM_SCHEMAS` | `Single(s)` ⇒ **unchanged `PLATFORM_SCHEMAS`-scoped scan** (re-critic confirmed correct; the port schemas were never in `PLATFORM_SCHEMAS` — §4.2/HIGH-3) ✓ |
| 5 | `foreign_schema_literal_in_body` `:716,738` (bare `%I` literal scan) | bare literal ∈ `PLATFORM_SCHEMAS` | `Single(s)` ⇒ **unchanged `PLATFORM_SCHEMAS` scan** ✓ |
| 6 | `check_func_def_target` reuse from `AlterFunctionStmt` `:279` | same as #2 | same projection ✓ |
| 7 | the executor's internal guard build (`executor.rs:890/1083/2558`) | `GuardConfig{project_schema: cfg.project_schema}` | now `GuardConfig::confined(cfg.project_schema)` when `cfg.trust == Confined` — same `Single(s)` ✓ |
| 8 | the **precondition** guard build (`precondition.rs:552-555`, via `executor.rs:992`/`:1112`) — HIGH-2 | `GuardConfig{project_schema: cfg.project_schema, ext: []}` | now `GuardConfig::confined(cfg.project_schema)` when `cfg.trust == Confined` — same `Single(s)`; latent today (loader sets `preconditions=[]`) ✓ |

The helper signatures change from `(…, project_schema: &str)` to `(…, scope:
&SchemaScope)`; the body adds one match arm (`Single(s)` = old logic; `Allowlist(v)`
= membership test). Sites 4 and 5 keep their `PLATFORM_SCHEMAS` lexical scan
verbatim under `Single` — and, per HIGH-3, even under `Allowlist` the port schemas
pass them unchanged (those schemas are not in `PLATFORM_SCHEMAS`).

**The regression test (§12 T2)** takes the **entire** existing
`tests/guard_security.rs` fixture set and re-runs it under
`GuardConfig::confined(...)`, asserting verdicts identical to the pre-change
baseline — and is **extended to explicitly cover the func-def-target (site 2) and
literal-schema-ref (site 3) fixtures**, not just the kind gate, since those are the
read sites the round-1 proof omitted. A second test (T2b) confirms the privileged
statements Platform now allows (`CREATE ROLE`, `GRANT`, `CREATE POLICY`,
`oauth_hydra.x` cross-schema, a `DO`-block `EXECUTE 'CREATE ROLE'`) are **still
`Denied`** under Confined.

---

## 7. Non-goals (deliberate, with citations)

- **No `DATABASECHANGELOG` adoption path; no `validCheckSum ANY` equivalent.**
  Pre-launch, no back-compat: AGENTS.md — *"Zeroship has never been published. No
  production users, no production tenants… there are no existing tables in
  production."* So there is **no DB whose Liquibase history must be honoured**.
  Fresh DBs re-migrate from scratch under the new engine; the engine never reads
  or writes `DATABASECHANGELOG`. The **2** `validCheckSum ANY` changesets
  (`0025_roles_rls.sql:84`, `0031_app_members_owner_backfill.sql:45`) exist
  *because Liquibase needed to mutate an already-applied changeset's body without a
  checksum failure* — a back-compat affordance we explicitly do not want. They get
  **authored in their final form** in the ported `V<NNNN>__` files (the body the
  `ANY` was covering for becomes *the* body), so there is nothing to re-checksum.
  This is the no-back-compat stance applied to migrations.
- **No dedicated least-priv "platform-migrator" role (for now).** §8.
- **No declarative-diff for platform schemas.** The platform schema is
  hand-authored SQL (it always was); we use the file loader, not the declarative
  author. The declarative/expand-contract engine is creator-only.
- **Hydra still migrates its own tables.** Unchanged — `hydra migrate` into
  `oauth_hydra` as the `oauth_hydra` role (`0027:10–14`). We only port the
  `oauth_hydra` **schema + role + grants + uuid-ossp pre-create**, which today live
  in `0027`.

---

## 8. Runs as the admin connection (least-priv platform role = future)

The Platform profile **runs as the admin connection** — i.e. `ExecutorConfig`
keeps `migrator_role: None` for platform applies (`db.rs:44–49`: "`None` runs as
the connecting (admin) role"). This is **not a regression**: Liquibase runs as
`postgres` (a superuser) today (`docker-compose.yml:90`,
`ops/db-migrate.sh:33`), and the platform migration *must* `CREATE ROLE` / `GRANT`
/ `CREATE EXTENSION`, which a NOSUPERUSER role cannot do. The admin connection is
the same privilege Liquibase already uses.

A dedicated, least-privileged **platform-migrator** role (e.g. `CREATEROLE` +
`CREATEDB` but `NOSUPERUSER`, scoped to the platform schemas) is **future
hardening**, tracked as a non-goal-for-now. It would tighten *below* Liquibase's
superuser, never above it — so deferring it leaves us strictly no worse than the
status quo. The RCE/host-escape guard backstop (§4) is the meaningful defense and
ships now.

### 8.1 Role passwords — status quo, and the loader's seam (H2)

<!-- New in round 2: addressing H2 — the ported files carry literal role
     passwords. Acknowledge them as ALREADY-committed dev literals (not a new
     exposure), specify the substitution seam, and track prod-secret handling. -->

The ported changesets carry **literal role passwords**:
`CREATE ROLE oauth_hydra LOGIN PASSWORD 'zeroship'` (`0027:24`) and the five
service-role passwords in 0025's DO-block (`zeroship_auth` …, `0025:127–139`,
each `PASSWORD 'zeroship_<role>'`). These are **already committed dev literals
today** — Liquibase applies them verbatim from the same changeset files. Porting
to `V<NNNN>__` files is byte-preserving, so this is **status quo, not a new
exposure** introduced by this design. We say so explicitly rather than silently
re-baking `PASSWORD 'zeroship'`.

What this design adds is the **seam** for handling them properly:

- **Substitution at load.** The loader supports `:var`-style placeholders
  (`PASSWORD ':oauth_hydra_password'`) resolved from environment variables /
  `--var name=value` flags at `migrate` time — the native equivalent of the
  Liquibase property-substitution the 0027 header already references
  (`0027:16–18`). A placeholder with no binding is a **hard load error** (fail
  closed; never apply a literal-looking placeholder as a password).
- **Or passwordless create + out-of-band set.** As both changesets already note
  (`0025:119–121`, `0027:16–18`), production provisions these roles and rotates
  their passwords **out of band**, at which point the `IF NOT EXISTS`-guarded
  `CREATE ROLE` no-ops. The migration can equally `CREATE ROLE … LOGIN` with no
  password and let the secret backend `ALTER ROLE … PASSWORD` it.

**Tracked follow-up:** wiring a real secret backend (the platform already stubs
vault / AWS-SM backends, per `project_config_hardening`) to the loader's `:var`
resolver, and removing the dev literals from the committed files once the dev
compose path reads them from env too. This is a non-goal **for this PR** (it does
not block the cutover and is orthogonal to the trust-profile work), but it is an
explicit, named debt — not an omission.

---

## 9. The CLI binary — `zeroship-migrate`

A new bin target in the crate (the crate is already compio-native; the CLI uses
**compio**, zero tokio — it `connect`s via `db.rs:connect`):

```
zeroship-migrate <SUBCOMMAND> --dir <PATH> --database-url <DSN> [--profile platform|confined]

  migrate    Apply all pending migrations (the compose `migrate` replacement).
             Requires --yes (or --allow-destructive) when the plan is destructive.
  status     Print applied vs pending (reads the journal), like Liquibase status.
  validate   Dry-run: load + guard-check every file + report checksum drift
             against the journal AND print destructive advisories. NO DDL.
             (Liquibase `validate` + `update-sql`.)
  rollback   Roll back to a target version via the .down.sql files (gated;
             requires explicit approval, mirrors engine.rs:710).

Args:
  --dir              Migration directory (default db/migrations/ post-port).
  --database-url     Postgres DSN (admin connection).
  --profile          platform (default for this binary) | confined. The ONLY place
                     `platform` is selectable; the CLI entrypoint lives in
                     `platform_runner` (§4.1), the one module that can mint a
                     `PlatformCapability` and thus call GuardConfig::platform(&cap,...)
                     / ExecutorConfig::platform(&cap,...) (§5). A creator request
                     never runs this binary and no creator-path module holds a token.
  --schema           Repeatable; the Platform schema allowlist (default: zeroship,
                     oauth_hydra, public).
  --yes / --allow-destructive
                     Required for `migrate` when the plan is destructive. Absent +
                     destructive ⇒ the CLI refuses, prints the advisories, exits
                     non-zero (does NOT apply).
  --project-id       Advisory-lock sentinel for serialization. Default "platform".
```

<!-- Revised in round 2: addressing H1 — the round-1 draft passed
     Approval::Approved unconditionally, throwing away the engine's working
     destructive gate. The CLI now honours it: --yes is REQUIRED to apply a
     destructive plan. -->
`migrate` flow (all inside `platform_runner`, the only token holder):
`let cap = PlatformCapability::new()` → `connect(dsn)` → build
`GuardConfig::platform(&cap, schemas, exts)` + `ExecutorConfig::platform(&cap, ...)`
(admin role, no `SET ROLE`, trust=Platform — §4.1) → load the dir into
`Vec<Migration>` (§6) → `let plan = engine.plan(set, platform_cfg)`. **Then the
destructive gate is honoured, not bypassed:**

- If `plan.destructive` (or `plan.requires_approval`) is **false** → pass
  `Approval::Approved` and apply (the common path: a fresh-DB full apply is purely
  additive — `CREATE`/`GRANT`/`CREATE POLICY` — so `destructive == false` and no
  `--yes` is needed).
- If `plan.destructive` is **true** and `--yes`/`--allow-destructive` was **not**
  given → **refuse**: print the destructive advisories + the offending versions and
  exit non-zero. Nothing applies. (`validate` should be run first to review them.)
- If `plan.destructive` is **true** and `--yes` **was** given → pass
  `Approval::Approved` and apply.

This keeps the engine's defense-in-depth gate intact: the executor re-checks
`Approval` (`executor.rs:596–600`), so even a buggy CLI cannot apply a destructive
batch without `Approved`. The operator's `--yes` is the explicit, auditable
confirmation — the CLI equivalent of re-submitting with approval, not a blanket
auto-approve. `rollback` is *always* destructive and *always* requires `--yes`
(`engine.rs:719–722`).

**The platform advisory-lock sentinel.** The executor serializes concurrent applies
on `pg_advisory_lock(hashtext(project_id)::bigint)` (`executor.rs:322–326`). Platform
migrations have no per-project id, so the CLI uses a **fixed sentinel
`project_id = "platform"`** (overridable via `--project-id`) when it builds
`ExecutorConfig`. Two concurrent `zeroship-migrate migrate` runs — the realistic
race is a **docker-compose restart that starts two `migrate` services**, or an
operator running it by hand while compose also runs it — therefore both hash to the
same advisory-lock key and **serialize**: the second blocks until the first commits
its journal, then its dedup read sees the applied set and no-ops
(`ApplyOutcome::is_noop`). A §12 T10 concurrency test asserts this. (`hashtext`'s
32-bit collision caveat, `executor.rs:315–321`, is irrelevant here: there is a
single well-known key, no unrelated ids to collide with.)

**Compose service swap** (`docker-compose.yml:81–95`): replace the
`liquibase/liquibase:4.31` image + `update` command with the platform image
running `zeroship-migrate migrate --dir /db/migrations --database-url
postgres://postgres:zeroship@postgres:5432/zeroship --profile platform`. It still
**runs once and exits**; `control`/`auth` keep gating on
`service_completed_successfully` (`docker-compose.yml:164–165`) — unchanged.

**`ops/db-migrate.sh` rewrite**: shell into the platform binary instead of the
Liquibase image; `status`/`validate`/`rollback` map to the subcommands above.
`update` → `migrate`; `update-sql` → `validate`; `changelog-sync` is **dropped**
(adoption is a non-goal, §7).

---

## 10. The port plan (56 files → `V<NNNN>__`)

Mechanical, one PR:

1. **Rename + split.** `0001_extensions_schemas.sql` → `V0001__extensions_schemas.sql`
   (body = the forward SQL, comments preserved as plain comments), and its
   `--rollback` lines → `V0001__extensions_schemas.down.sql`. Repeat for all 56,
   preserving the numeric prefix verbatim (the `0045` gap is fine). A file with
   **multiple `--changeset`s** becomes **one** `V<NNNN>__` file (multi-statement,
   whole-file atomic) — the per-changeset boundaries vanish (we don't need them;
   the file is the unit).

   <!-- New in round 2: addressing M3 — a Liquibase file has N changesets each with
        its own --rollback applied in REVERSE order; the single .down.sql must
        concatenate them in REVERSE-changeset order, or rollback corrupts state. -->
   **Down-file concatenation is REVERSE-changeset order.** A multi-changeset file's
   `.down.sql` is the per-changeset `--rollback` blocks concatenated **in reverse
   changeset order** — because rollback must undo the last-applied changeset first.
   Worked example, `0025_roles_rls.sql` (changesets in apply order: roles → 4× RLS
   tables; rollbacks at `:241`, `:255–257`, `:266–268`, `:277–279`, `:291–293`).
   The `V0025__roles_rls.down.sql` is:
   ```sql
   -- (reverse order) undo app_user_identities RLS, then anchors, then sessions,
   -- then app_secrets, then finally drop the roles.
   DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_user_identities;
   ALTER TABLE zeroship.app_user_identities NO FORCE ROW LEVEL SECURITY;
   ALTER TABLE zeroship.app_user_identities DISABLE ROW LEVEL SECURITY;
   -- … app_session_anchors, gateway_sessions, app_secrets (same triplet) …
   DO $rb$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_auth') THEN
     EXECUTE 'DROP OWNED BY zeroship_auth, zeroship_control, zeroship_gateway, zeroship_worker, zeroship_app';
     EXECUTE 'DROP ROLE IF EXISTS zeroship_auth, zeroship_control, zeroship_gateway, zeroship_worker, zeroship_app';
   END IF; END $rb$;
   ```
   i.e. **drop policies → disable RLS → drop roles** (the inverse of create roles →
   enable RLS → create policies). Every construct here is in the §4.1 flip table's
   `.down.sql` column, so the Platform rollback guard admits it.

   **Tradeoff acknowledged:** collapsing N changesets into one `.down.sql` loses
   Liquibase's *per-changeset* partial-failure diagnosis (Liquibase tracked which
   changeset's rollback failed). With the whole-file unit, a rollback runs the
   `.down.sql` as one transactional batch; a mid-file failure rolls the whole down
   back and **still surfaces the exact failing statement** via
   `ApplyError::MigrationFailed { version, source }` (the DB error from the failed
   statement, `executor.rs:294–303`). So the diagnosis is statement-level, just not
   changeset-labelled — an acceptable loss given the file is the atomicity unit.

   **Platform `rollback` walks files in reverse file-version order.** `rollback to
   V<target>` applies the `.down.sql` of each applied file with version `>` target,
   **in descending `FileVersion` order** (`V0057.down` before `V0056.down` …),
   mirroring the within-file reverse-changeset rule one level up. This is the
   existing engine rollback ordering (`executor::rollback` orders by version desc);
   the loader just supplies the file-version ordering key (§6.2).
2. **`validCheckSum ANY` collapse** (`0025`, `0031`): drop the directive; keep the
   final body. Author it as the one true body (§7).
3. **`splitStatements` / `runOnChange` directives**: dropped. `splitStatements`
   was a Liquibase parser hint; our engine parses with `pg_query` and runs the
   whole file. `runOnChange` (if any) → an `R__` file.
4. **The Liquibase tracking comment lines** (`--liquibase formatted sql`,
   `--changeset`, `--rollback`) are stripped from `up` bodies (the reverse moves to
   `.down.sql`).
5. **Delete the changelog**: `db/changelog/db.changelog-master.yaml` and the whole
   `db/changelog/changesets/` tree are removed in the same PR; the new files live
   under `db/migrations/`.
6. **Compose + ops swap** (§9).

Per the no-back-compat invariant, this is **one PR** — no dual-run, no
"Liquibase-and-also-ours" interim, no detect-and-warn. The changelog is deleted,
not deprecated.

---

## 11. Phased implementation

Each phase is independently reviewable + testable; phases 1–3 land the engine
capability, 4–6 land the platform cutover.

1. **Phase 1 — `GuardConfig` Platform profile (the security core).** This phase is
   bigger than the round-1 draft scoped it; it must land **all** of:
   - `TrustProfile` (`#[non_exhaustive]`, `Platform` crate-private — §4.1, C1),
     the **`PlatformCapability` token + private `platform_runner` submodule**
     (HIGH-1), `SchemaScope`, the `confined()`/`platform(&cap, …)` constructors, and
     **privatise the `trust`/`schemas`/`extension_allowlist` fields** — which forces
     the **14-site struct-literal → `confined(...)` refactor** crate-wide (§4.1,
     MINOR-1; the count must be exact for the compiler-forces-completeness argument).
   - The **complete privileged-kind flip table** in `check_statement_kind`
     (every row of §4.1, incl. the `.down.sql`-only drops + the four RLS subtypes +
     the trust-aware `is_safe_drop_object`/`is_safe_alter_table_subtype`) — C3. The
     `GrantStmt` arm is allowed **wholesale** under Platform; do NOT add a per-objtype
     filter (the objtype column is descriptive — MINOR-3).
   - The **body-scan widening** (§4.2, C2): conditionalize the `create role`/`drop
     role`/`search_path` **token-scan** needles on `trust == Platform`, keeping the
     RCE/host-escape token scans hard. Verify the recursion arm already propagates
     `self.cfg`. **Do NOT bother relaxing the body foreign-schema helpers for the
     port** — they scan only `PLATFORM_SCHEMAS = {control,auth,billing}`, which the
     port schemas (`zeroship`/`oauth_hydra`/`public`) are not in, so they already
     pass (HIGH-3); a `&SchemaScope` signature change for §6.4 uniformity is fine but
     leaves port behaviour unchanged. The `allowlist ∪ PLATFORM_SCHEMAS` form is a
     future-platform-body provision, not this PR.
   - The multi-schema `SchemaScope`-aware `check_cross_schema` + the **eight** read
     sites of §6.4 (H3 + HIGH-2 precondition site).
   - **`ExecutorConfig` trust threading** (§4.1, §5.4): `pub(crate) trust` field +
     `ExecutorConfig::platform(&cap, …)`, and the **four** executor-path guard sites
     (`890`/`1083`/`2558` + `precondition.rs:552` reached from `executor.rs:992`/
     `:1112`) building `confined`/`platform` from it — HIGH-2.
   - The T8 trybuild compile-fail test (L2), the **T11 in-crate token unit**
     (HIGH-1), and the T2/T2b Confined-unchanged regression. Confined default
     unchanged. (§4, §4.1, §4.2, §6.4.)
2. **Phase 2 — file loader + versioning.** Parse the directory into
   `Vec<Migration>` (`V`/`R`/`.down.sql` grammar §6.1, the deterministic
   `FileVersion → MigrationId` derivation §6.2, checksum + flags via the
   `submit.rs` model). Pure, DB-free, unit-testable.
3. **Phase 3 — CLI binary.** `migrate`/`status`/`validate`/`rollback` over the
   loader + engine, compio `connect`. (§9.)
4. **Phase 4 — port the 56 changesets** to `V<NNNN>__`/`.down.sql`/`R__`. (§10.)
5. **Phase 5 — compose + `ops/db-migrate.sh` swap; delete the changelog.** (§9–§10.)
6. **Phase 6 — e2e on real Postgres.** Boot the stack with the new `migrate`
   service against a fresh DB; assert the full schema materializes and
   `control`/`auth` boot green.

---

## 12. Test strategy (faithful, real-PG)

Per `feedback_faithful_e2e_tests` (run the REAL path, no shims):

- **T1 — fresh full apply (real PG).** Load all ported `V<NNNN>__` files and
  `engine.apply` them under the Platform profile against a clean Postgres
  (the `:5440` dev DB). Assert: every file applies, the resulting schema matches
  the Liquibase-built schema object-for-object (introspect `pg_namespace`,
  `pg_class`, `pg_proc`, `pg_policy`, `pg_roles` for `zeroship_*`/`oauth_hydra`),
  RLS is `FORCE`d on the four tenant tables, and the journal has one row per file.
  This is the §11 Phase-6 gate and the faithful replacement for the Liquibase
  `migrate` service.
- **T2 — Confined-default-unchanged regression (§6.4, H3).** Re-run the entire
  existing `tests/guard_security.rs` fixture set under `GuardConfig::confined(...)`
  and assert verdicts are identical to the pre-change baseline. **Explicitly
  includes func-def-target fixtures** (`CREATE FUNCTION public.f()` /
  `ALTER FUNCTION control.f()` → CrossSchema, site 2 of §6.4) **and
  literal-schema-ref fixtures** (`'control.t'::regclass`, `nextval('control.s')`,
  namespace resolvers → CrossSchema, site 3) — the read sites the round-1 proof
  omitted — not just the kind gate. **T2b:** a privileged statement (`CREATE ROLE`,
  `GRANT`, `CREATE POLICY`, `ENABLE/FORCE ROW LEVEL SECURITY`, `DROP POLICY`,
  `oauth_hydra.x` cross-schema, and a `DO`-block `EXECUTE 'CREATE ROLE …'`) is
  **still `Denied`** in Confined.
- **T3 — trust-boundary test (§5).** Assert the submission path cannot reach
  Platform: (a) `Submission` has no trust/profile field and `submit_migration` only
  constructs `confined(...)` (covered structurally + by T8); (b) a runtime test that
  a privileged `up` submitted through `submit_migration` returns
  `SubmissionOutcome::Denied` — i.e. submission gets the Confined guard regardless
  of input.
- **T4 — Platform widening is correct AND bounded.** Under Platform: every §4.1
  flip-table construct (top-level AND `.down.sql`-only: `DROP POLICY`/`DROP
  ROLE`/`DROP SCHEMA`/`DROP EXTENSION`/`NO FORCE RLS`/`DISABLE RLS`/`DROP OWNED BY`)
  now **passes**; but `COPY … PROGRAM`, `pg_read_file`, `dblink`, untrusted PLs,
  `ALTER SYSTEM`, `SECURITY DEFINER`, and a cross-schema reference to a **creator**
  schema (`proj_acme.x`, outside the allowlist) are **still `Denied`**. (Proves
  "lint-mode-PLUS-backstop", not "lint-only".)
- **T4b — DO-block privileged DDL applies under Platform (C2).** The literal 0025
  `DO $bootstrap$` block (`EXECUTE 'CREATE ROLE …'` + `EXECUTE 'ALTER ROLE … SET
  search_path …'` + `EXECUTE 'GRANT …'`) and the 0027 `DO $$ … CREATE ROLE
  oauth_hydra …'` block both **pass** under Platform — proving the body-scan
  widening (§4.2) reaches BOTH the recursion arm and the token-scan/`search_path`
  substring arm. T4b-neg: the SAME blocks are **`Denied`** under Confined (the
  token-scan stays hard), and a `DO $$ … COPY x FROM PROGRAM 'curl …' $$` is
  **`Denied`** even under Platform (RCE token-scan kept).
- **T5 — checksum drift on a ported file.** Apply T1, mutate one `V<NNNN>__` file
  body, re-run `validate`/`migrate`; assert a hard drift abort (the engine's
  per-migration drift check fires — §3 axis 2).
- **T6 — rollback (real PG).** Apply, then `rollback` to a target version via
  `.down.sql`; assert each reverse applies **under the Platform guard** (so the
  `.down.sql`-only flip-table rows are exercised — `DROP POLICY` → `DISABLE RLS` →
  `DROP ROLE` for V0025), files roll back in **descending file-version order**
  (§10), and each journals a `rolled_back` event.
- **T7 — idempotent re-run.** `migrate` twice; second run is a no-op
  (`ApplyOutcome::is_noop`).
- **T8 — trybuild compile-fail (C1, L2).** A `trybuild` UI test in an external test
  crate asserts that `GuardConfig { trust: TrustProfile::Platform, .. }` and naming
  `TrustProfile::Platform` (in a literal, a match, or as an argument) **do NOT
  compile** outside the crate — and specifically not from a module that mirrors
  `submit`'s imports. This is the static proof of the §5 invariant.
- **T9 — version→id determinism + ordering round-trip (M1/M2).** Unit test for
  `migration_id_for_version` (§6.2): (a) same `V<NNNN>` → same `MigrationId` every
  call; (b) the derived id `MigrationId::parse`-round-trips; (c) `V1 < V2 < … <
  V10000` holds in BOTH `FileVersion` and the derived `MigrationId::Ord`; (d)
  duplicate `V<NNNN>` prefixes in a dir are a hard load error. Asserts the
  fixed-22-width/ascending-alphabet invariant is what carries ordering.
- **T10 — platform advisory-lock concurrency (§9).** Two `migrate` runs against the
  same DB with the same `project_id = "platform"` sentinel started concurrently
  serialize on `pg_advisory_lock(hashtext("platform"))`: one applies, the other
  blocks then returns `is_noop`. Simulates the compose-restart double-`migrate`
  race; the real DB + journal show exactly one applied set.
- **T11 — in-crate capability construction is `platform_runner`-only (C1/HIGH-1).**
  An **in-crate** unit test (it lives outside `platform_runner`, in the crate's
  test module) asserts that `PlatformCapability::new()` is **not reachable** from a
  sibling module — the test attempts to call `GuardConfig::platform(...)` /
  `ExecutorConfig::platform(...)` and finds it cannot, because it cannot construct a
  `PlatformCapability` (the `new` is `pub(super)` to `platform_runner`). Conversely
  a positive unit *inside* `platform_runner` mints the token and builds a Platform
  `GuardConfig`/`ExecutorConfig`, proving the constructor works for the one module
  that should hold it. Together with T8 (external trybuild) this pins **both**
  boundaries: T8 = external un-nameable, T11 = in-crate un-mintable. (Mechanically,
  the negative half is a second trybuild fixture compiled as if it were a sibling
  in-crate module, since "does not compile" is the assertion.)

---

## 13. Risks

- **R1 — the Platform widening is too broad / too narrow.** Mitigated by §4's
  explicit "kept-denied" list, the **complete, code-verified §4.1 flip table** (top-
  level + body + `.down.sql`-only constructs), and T4/T4b. The widening is
  *enumerated* (the exact kinds that flip, each cited to a `guard.rs` line or
  predicate), not a blanket "allow if Platform"; everything not in the flip table
  stays denied-by-default (`guard.rs:373`) — including the RCE/host-escape token
  scans the §4.2 body widening explicitly keeps hard. *Too narrow* now surfaces as a
  T1/T6 apply/rollback failure (the executor static first-pass denies before DDL),
  not a silent allow — and the `.down.sql`-only rows close the round-1 hole where
  rollback would have been impossible.
- **R2 — `FileVersion → MigrationId` derivation collides or mis-orders.** Mitigated
  by the single canonical `migration_id_for_version` (§6.2) packing the version into
  the high 48 bits with zero low bits, **relying on the load-bearing fixed-22-width
  ascending-alphabet base62 invariant** (`typed_id.rs:10–12,27–38`) so larger
  version ⇒ lexicographically larger id. T9 asserts determinism, `parse`
  round-trip, and `V1 < V2 < … < V10000` ordering; a future variable-width encoder
  would fail T9 loudly rather than silently mis-order. version > 2^48 is unreachable
  (loader bound; 56 files ≤ 0057). Duplicate `V<NNNN>` prefixes are a **hard load
  error**.
- **R3 — multi-statement whole-file atomicity hides a non-transactional stmt.** A
  ported file containing `CREATE INDEX CONCURRENTLY` (none in the 56 today) cannot
  run inside the transactional path. Mitigated by `flags_for` **auto-deriving**
  `transactional` per file (§6.3, M4): the classifier flags the non-txn statement
  and the file takes the existing two-phase path (`executor.rs:472`) with its
  idempotency requirement (`CONCURRENTLY` must be `IF NOT EXISTS`, etc.) — **no new
  per-file marker**. Platform files stay transactional where no idempotent-recovery
  form exists (`CREATE POLICY` has no `IF NOT EXISTS`); verified none of the 56 is
  non-txn today, so all take the txn path.
- **R4 — extension allowlist breadth / `public`-schema breadth.** The Platform
  extension allowlist is exactly `[citext, uuid-ossp]`. Both are **absent from
  `FORBIDDEN_EXTENSIONS`** (verified — the list is `dblink, postgres_fdw, file_fdw,
  mysql_fdw, oracle_fdw, tds_fdw, plpython*u, plperlu, pltclu, plsh, plr, plv8,
  adminpack, amcheck, pg_background, lo`, `denylist.rs:37–56`), so the allowlist is
  honoured; and `FORBIDDEN_EXTENSIONS` still override the allowlist in **both**
  profiles (`guard.rs:291`), so a future allowlist typo of e.g. `dblink` is still
  denied (L3). `public` is in the schema allowlist (`oauth_hydra` grants
  `USAGE`/extensions there, `0027:33`); the allowlist permits *references* to
  `public`, but the RCE/file/network backstop and `FORBIDDEN_EXTENSIONS` still
  apply, so it is not a hole. The creator path never has `public` in scope (its
  `migrator` role REVOKEs `public`, `role.rs:294`).
- **R5 — losing Liquibase's `changelog-sync`/`tag`.** Dropped by design (§7, no
  adoption). The only consumer was adopting a pre-existing DB, which pre-launch
  does not have.
- **R6 — the profile reaches the planner but NOT every executor-path guard.**
  This was a real gap: `engine.plan` takes a `&GuardConfig`, but the executor
  re-derives its own guards from `ExecutorConfig.project_schema` at **four** sites —
  the versioned/repeatable static first-passes (`executor.rs:890/1083`), `rollback`
  (`:2558`), **and the precondition guard** (`precondition.rs:552`, reached from
  `executor.rs:992`/`:1112`, HIGH-2) — so a Platform plan would have been re-denied
  by an executor-path Confined guard. Mitigated by threading a `pub(crate)` `trust`
  onto `ExecutorConfig` (§4.1, §5.4) so **all four** are built `platform` iff the
  operator constructed an `ExecutorConfig::platform(&cap, …)`. The precondition site
  is latent for the 56-file port (loader sets `preconditions=[]`) but must be
  threaded so a future platform precondition referencing a platform schema is not
  wrongly Denied. T1/T6 (real-PG apply + rollback) would have failed loudly had the
  first three been missed; the precondition site is covered structurally + by a
  Platform-precondition unit (extend T4).

---

## 14. Summary

Add a **Platform** trust profile to the existing `zeroship-migrate` guard
(privileged DDL allowed for the platform schemas, RCE/host-escape backstop kept),
plus a native **Flyway-style file loader** and a compio **CLI** — and run the
platform's own schema (`zeroship` + `oauth_hydra`) on our engine instead of
Liquibase. Trust separation, which Liquibase enforced *physically* (a different
binary), is replaced by a stronger **call-site invariant** that is now precisely
scoped: **externally** `TrustProfile::Platform` is `#[non_exhaustive]`/crate-private
and `GuardConfig`'s fields are private, so no other crate can name or forge it (by
construction, the real threat boundary); **in-crate** `platform()` requires a
`PlatformCapability` token mintable only inside a private `platform_runner`
submodule, so even sibling modules (`submit`/`engine`) cannot mint Platform — Platform
is a **capability handle granted by holding the token**, not a field a struct literal
can forge nor a `pub(crate)` fn a sibling could simply call. The creator
`submit_migration` ingress is hard-wired to `confined(...)`, and the same
private-field + token discipline on `ExecutorConfig` carries the profile into **all
four** executor-path guards — the three first-pass/rollback guards **and the
precondition guard** the round-2 draft missed. A trybuild compile-fail test pins
that an external crate can never even *name* Platform, and an in-crate unit pins
that only `platform_runner` can mint the capability. The widening is a **complete, code-verified flip
table** spanning the top-level kind gate, the DO-block body scan (so 0025/0027's
`DO`-block role/grant/RLS DDL applies under Platform while the RCE/host-escape token
scans stay hard), and the `.down.sql`-only reverse constructs — while the unchanged
NOSUPERUSER `migrator` role remains the orthogonal line-2 defense and the §3 win is
scoped honestly to the parse-time backstop + checksum integrity (privilege posture
unchanged). The CLI honours the engine's destructive gate (`--yes` required) and
serializes concurrent runs on a fixed `"platform"` advisory-lock sentinel.
Pre-launch no-back-compat means a clean cutover: 56 changesets → `V<NNNN>__` files
(per-file `transactional` auto-derived, reverse-changeset-order `.down.sql`), the
`validCheckSum ANY` bodies authored in final form, role-password literals
acknowledged as status-quo dev secrets with a tracked substitution seam, the
changelog deleted, the compose `migrate` service swapped to the binary, and fresh
DBs re-migrating from scratch under one audited, zero-tokio engine.
