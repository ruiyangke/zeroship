# zeroship-migrate — Design & Architecture

`zeroship-migrate` is zeroship's versioned database migration engine. It authors schema changes in a portable, dialect-neutral JavaScript DSL (`@zeroship/migrate`), validates them against a real SQL parser and a least-privilege database role, and applies them through a native compio/io_uring stack across PostgreSQL, SQLite, and MySQL. It is the sole migration mechanism for both the platform's own schema and creator project databases — one engine, two trust profiles.

This is the consolidated, authoritative design record. The detailed API reference lives in `docs/reference/zeroship-migrate-guide.md`; intentional dialect divergences in `docs/reference/sqlite-divergences.md`.

---

## 1. Principles

- **The IR is the contract.** Every migration compiles to a closed, dialect-neutral, SHA-256-checksummed intermediate representation. The IR — not the JS source or the rendered SQL — is the authoritative artifact that travels across dev, build, and deploy. There is no hidden SQL divergence.
- **No raw SQL in expression position.** The expression algebra is a closed AST. Raw SQL survives only in three explicitly-marked, capability-gated islands (function bodies, view bodies, the `raw()` escape), each carrying a required `reason` that is itself checksummed.
- **Untrusted by construction.** Migrations are privileged DDL authored by untrusted creators and by a prompt-injectable AI. Defense is in depth: a parse-time deny-list plus a least-privilege database role, backstopping each other.
- **PostgreSQL is first-class.** The core surface is PostgreSQL-shaped. Portability to SQLite/MySQL is explicit and opt-in; anything with no native realization and no portability leg fails closed at that target.
- **Fail closed, never silent.** Unsupported constructs, volatile functions in immutable positions, aggregates in scalar positions, and off-dialect operations are all rejected with structured errors before apply — never silently skipped.
- **One engine, many backends.** A single generic orchestrator handles versioning, locking, recovery, and gating; dialect-specific machinery lives only behind a backend trait.

---

## 2. The migration unit

**Versioned, checksummed, immutable.** Migrations are ordered by UUIDv7 typed ids (`mig_<base62>`) minted at authoring time, not sequential numbers — the time-ordered high bits let concurrent multi-app authoring produce collision-free ids that string-sort into apply order. Each migration carries `up`, an optional `down`, a checksum over both, and immutable flags (transactional, destructive, online, requires-approval). The checksum is recomputed on every apply; a mismatch — an edit in flight, or schema drift — aborts immediately. (`crates/zeroship-migrate/src/model/migration.rs`, `apply/executor.rs`.)

**Deterministic sub-version derivation.** Multi-step sequences (expand-contract renames, staged backfills) derive sub-step ids deterministically from the parent id plus a step index (SHA-256, with a high-bit marker that prevents collision with authored versions). Re-lowering the same artifact reproduces byte-identical ids, so cross-deploy obligation keys, idempotent skips, and auto-discharge all remain stable.

**The immutable journal.** State lives in an append-only, tamper-evident event log (`<meta_schema>.schema_migrations`), guarded by an immutability trigger. Every apply or rollback emits one event row (version, name, checksum, actor, timestamp, duration, phase, kind, outcome); event order is fixed by a native identity sequence, and the net state of a version is its latest event. Non-transactional DDL uses a separate mutable inflight side-table for started/completed markers, dropped on success. (`apply/journal.rs`.)

---

## 3. Security model

Migrations execute privileged DDL from untrusted sources, so security is layered and enforced at more than one level.

**Line 1 — the parse-time guard.** Every statement is parsed with the real PostgreSQL parser (`libpg_query`, chosen precisely so exotic syntax cannot slip a deny-list) and checked against a hard deny-list: remote code execution (`COPY … PROGRAM`, untrusted procedural languages, C functions, file/network paths), privilege escalation (`CREATE ROLE`, `GRANT`, `ALTER SYSTEM`), and cross-schema access outside the declared project schema. Dangerous constructs nested inside `DO $$…$$` blocks and function bodies are inspected too. Unparseable input is denied. The guard runs out of band at submit/deploy time, so it is plain synchronous, exhaustively testable logic. (`crates/zeroship-migrate/src/guard/`.)

**Line 2 — the least-privilege role.** Each project gets a `migrator_<project>` role that is `NOLOGIN` (the executor connects as admin and `SET ROLE`s into it), `NOSUPERUSER NOCREATEROLE NOCREATEDB`, owns and is `search_path`-pinned to the project schema (plus extension schemas for lookup), and has no access to the meta schema or any other project's schema. Even SQL constructed at runtime (`DO`/`EXECUTE` bodies that evade static analysis) hits `permission denied` at the database privilege layer. This is why the guard gates the *execution surface* (the presence of a dangerous carrier) rather than trying to statically detect every dangerous op — the database is the backstop. (`apply/role.rs`.)

**Trust profiles, enforced by type.** Three profiles exist:
- **Confined** — the default anyone can construct with no token. Restricted to a single project schema, deny-by-default on vendor operations (roles, policies, grants, extensions).
- **Platform** — for zeroship's own schemas; widens the deny-list to allow role/grant/policy/schema management, but only for a fixed operator-supplied schema allowlist. RCE and host-escape surface stays hard-denied.
- **Trusted** — a permissive dbmate-style posture for standalone operation.

Platform and Trusted are constructible **only** by holding an `OperatorCapability` token — a zero-sized type whose sole constructor lives in a named operator seam. Config fields are private; the only public constructors are `confined()` (safe) and `platform()`/`trusted()` (token-gated). The creator submission path hard-wires `confined()` and has no path to the token, so a creator migration can never be applied under Platform — a statically-enforced invariant, strictly tighter than physical tool separation. (`crates/zeroship-migrate/src/model/capability.rs`, `guard/`.)

**Threat model.** The engine assumes untrusted creator SQL, prompt-injectable AI-authored SQL, runtime-constructed SQL that can evade parse-time checks, and in-flight edits after submission. The layered response: parse gate, cross-schema confinement, immutable checksummed journal with per-apply re-verification, and statement/lock timeouts that bound resource use.

---

## 4. The apply engine

**Project serialization.** The executor takes a project-scoped advisory lock at the start of apply and holds it to the end, so concurrent deploys queue rather than corrupt the journal. Declarative deploys acquire the lock once and pass "already held" into their sub-batches, serializing the whole deploy as one unit. (`apply/executor.rs`.)

**Two-phase apply.** Transactional migrations (the safe default) run `BEGIN; SET LOCAL <timeouts, role>; <up>; INSERT journal; COMMIT` — DDL and journal commit atomically, so a crash leaves both or neither. Non-transactional migrations (opt-in, e.g. `CREATE INDEX CONCURRENTLY IF NOT EXISTS`) run two-phase: write a started marker, run the required-idempotent `<up>`, write the immutable completed row, drop the marker. A crash leaves a lone started marker, which the next apply detects and re-runs safely.

**Bounded resources.** `statement_timeout` (default 60s) bounds how long a statement runs; `lock_timeout` (default 3s) bounds how long it waits for a lock before failing fast — deliberately split so a long backfill gets time to run once it holds the lock, while blocking DDL fails quickly instead of stalling a live tenant. Lock-timeout failures are retryable, and the two-phase recovery handles them cleanly. (`crates/zeroship-migrate/src/conn.rs`.)

**Checksum re-verification, baseline, rollback.** Every apply re-checks the checksums of already-applied migrations and hard-aborts on mismatch rather than auto-fixing drift. `baseline` records an existing schema as a starting point without replaying history. Rollback applies `down`s in reverse to a target version; the engine favors roll-forward compensating migrations for old destructive history (down-ing a past `DROP`/`TRUNCATE` is riskier than a new compensating step), and true rollback is explicitly gated. (`apply/executor.rs`, `apply/baseline.rs`.)

---

## 5. Project model & platform self-hosting

**Project umbrella.** A project is one shared database schema plus one or more apps. Apps share the schema — the schema is the union of all member apps' declared tables. One app *owns* a table's structure (it declared it); any app may *use* (read/write rows in) a table it did not declare. Identical re-declaration is idempotent; conflicting re-declaration is a deploy error; a removed app never auto-drops its tables. Inter-app ordering uses UUIDv7 plus an optional `depends_on` (so an app's foreign key to another app's table applies in the right order). (`model/ir.rs`, `plan/`.)

**Platform migrations are DSL, not SQL.** The platform's own schema (control, auth, billing, and related schemas) is migrated by the same engine, authored in the same TypeScript DSL, committed and reviewed in the repository (`db/migrations-ts/`). They run under the Platform profile with a schema allowlist; creator migrations run confined. The executor logic — journal, advisory lock, two-phase recovery — is identical; only the guard profile and schema scope differ. There is no separate migration tool for the platform.

---

## 6. The authoring surface

The `@zeroship/migrate` DSL is the creator-facing (and platform-facing) authoring layer that produces the IR. Its full API is in the reference guide; the durable design decisions:

**Fluent `table()`, one handle.** `table(name, {schema?})` returns an inert handle that records nothing until a terminal is invoked; every operation returns the same handle, so both chained and variable-held styles are first-class. There is one handle — no `pgTable` variant, no `/pg` subpath — carrying every operation (portable and PostgreSQL-vendor alike), gated at validation by the engine's capability and dialect checks.

**Receiver-first chains; value constructors as imports.** Every operator and receiver-ful function is a chain method on the expression (`col("x").eq(1)`, `col("s").lower().regex(…)`, `col("amount").sum()`) — one uniform model, no operator namespaces. Receiver-less value producers (`now()`, `genRandomUuid()`, `currentSetting()`, `interval()`, `nextval()`, `lit()`) are top-level imports, so simple defaults need no callback: `t.timestamp().default(now())`. `COUNT(*)` is the receiver-less `countStar()`.

**The `t.*` type lexicon, unified with `@zeroship/db`.** Column types are authored through an immutable `t.*` chain (`t.text()`, `t.uuid()`, `t.vector({dimensions, metric})`, `t.encrypted({of})`, …); every modifier returns a fresh definition, never mutating. The lexicon is the same token set `@zeroship/db` generates, bridged one-way by `fromDb()`, so migration and query types stay in lockstep. Payload numbers are named (`t.char({length})`, `t.numeric({precision, scale})`).

**PostgreSQL-first, portability opt-in.** PostgreSQL constructs — `schema()`, `role()`, `extension()`, grants, RLS/policies, triggers, sequences, domains, the full index-method set (`gin`/`gist`/`brin`/`hnsw`/…), `EXCLUDE`, `ON CONFLICT`, `~` regex — are first-class on the core surface. Portability is declared where a construct diverges, via a context-aware `dialect({ pg, sqlite, mysql, default })` combinator that works at expression granularity (a divergent predicate) and at op/spec granularity (a whole index or column that differs, or is absent on a target when its leg is omitted). `dialect()` is a declarative value-level assertion of per-dialect alternatives, not author-time branching — `up()` receives no dialect parameter. Each leg is validated under its own dialect.

**Fail-at-the-earliest-layer.** Context-typed builders reject misuse at compile time where they can (a default can't reference a column; aggregates only where a grouped context admits them). What compile-time types can't catch, a validate-time backstop does: volatile functions in immutable positions, aggregates in scalar positions, and operations on dialects that lack a native form (with no `dialect()` leg) are all rejected with structured errors before apply.

**One intent, one terminal.** Column alterations use distinct terminals (`.setType()`, `.setNotNull()`/`.dropNotNull()`, `.setDefault()`/`.dropDefault()`, `.rename()`, `.comment()`) rather than an options bag that could silently drop a change. Named sub-objects (columns, constraints, indexes, policies, triggers, partitions) are addressed through selectors (`.column(n)`, `.foreignKey(n)`, `.index(n)`, …) whose terminals record eagerly and return the parent handle. A selector handed out but never terminated fails the build (`SELECTOR_NOT_TERMINATED`); one terminated twice also fails.

**Named payloads, positional identity only.** Every operation takes one positional argument — the name — and one named options object; no positional booleans or enums. Schema is stated once and inherited, with per-op override only where scope is ambiguous. A declaration lint enforces this over the built type surface.

**Raw as a counted debt instrument.** The three raw-SQL islands — `raw({sql, reason})`, `rawSelect({sql, reason})`, and function bodies — each carry a `reason` that is checksummed into the artifact. Raw and `dialect()`-override counts are ratcheted against a committed baseline: increases need a waiver, decreases auto-lower the baseline. Raw usage is always visible and auditable.

---

## 7. Multi-dialect architecture

**One IR, a backend trait.** A single generic orchestrator drives apply through a `MigrationBackend` trait; there is no forked executor. Partition logic, squash/expand gates, the two-phase protocol, and the repeatable phase are written once; connection semantics, journal SQL, and schema introspection live behind the trait. (`apply/backend/`.)

**Three backends.**
- **PostgreSQL** — the rich reference, over the native compio-postgres driver: roles, `search_path`/GUC control, `information_schema`/`pg_catalog` drift introspection, `pg_advisory_lock` serialization, and the two-phase non-transactional path. Its output is the byte-identical regression bar.
- **SQLite** — an in-process hardened actor for the dev tier (zero external infrastructure): extension-load disabled, `DEFENSIVE` + `TRUSTED_SCHEMA=OFF`, and a runtime two-mode authorizer (creator-up vs engine-journal) flipped at the statement boundary in place of roles. Journal atomicity uses a shared counter table with `RETURNING`.
- **MySQL** — the real `mysql2` npm driver bridged in a Trusted V8 isolate over `node:net`, avoiding a bespoke compio-mysql driver. It declares its DDL non-transactional (MySQL auto-commits DDL), so every migration flows through the two-phase inflight-marker path.

**The dialect table.** A generated, single-source disposition matrix maps each `(op-kind, variant)` to one of Portable / Vendor / TransparentDegradable / Unsupported per dialect. It is authored in a hand-reviewed sidecar (`dialect-support.toml`), regenerated into both a Rust const and a TS mirror, and consulted at render time. Dialect knowledge lives in this one declarative table, not scattered across rendering sites; a faithfulness test pins the table against the engine's live support decision.

**Fail-closed dialect gate.** When a construct's disposition is Unsupported on the target, the renderer emits a `DIALECT_UNSUPPORTED`/`EXPR_NOT_PORTABLE` error before any application — a silent skip would leave the schema unsound. The author either keeps to portable constructs or spells the divergence explicitly with `dialect()`.

**Journal-atomicity as a declared capability.** Not every engine can commit DDL and its journal row in one transaction. Each backend answers `ddl_is_transactional()`; the executor dispatches to the transactional path or the two-phase inflight-marker path accordingly. This lifts MySQL's auto-commit-DDL property from a special case to a capability the executor honors uniformly, and it is the shape that scales to a fourth dialect.

**Intentional divergences.** PostgreSQL↔SQLite differences in vector metrics, full-text tokenization/scoring, spatial search, transaction isolation, and text ordering are documented contracts, not regressions — SQLite is the zero-infrastructure dev tier and its search/isolation model differs by design. Documenting them prevents dev-vs-prod behavior surprises. (`docs/reference/sqlite-divergences.md`.)

---

## 8. Schema authority: declarative desired-state

The engine owns all DDL. Beyond hand-authored migrations, it supports a declarative desired-state path: a creator declares a schema descriptor; `generate` diffs the descriptor against the journal/live schema and emits a committed, reviewable versioned migration; apply runs it confined. `@zeroship/db` infers query types from the same descriptor, so there is no separate codegen step.

The diff/lower split is deliberate: comparison (desired vs introspected snapshot) is dialect-neutral and produces an intent (`Diff` of additions, renames, rebuilds); lowering renders that intent to dialect-specific DDL and routes structural changes by capability (a rename becomes online expand-contract on PostgreSQL, an offline table rebuild on SQLite). The expensive, smart part is written once; a new dialect plugs in as a renderer, and structural constraints (SQLite has no online rename) hold by construction rather than a runtime check. (`render/declarative.rs`, `render/lower.rs`.)

---

## 9. Notable features

- **Partitioned tables** — a parent strategy plus first-class child operations (`create({ partitionBy: … })` + `partition(name).of(parent).forValues(…)`); partition is a dedicated op, not a `CreateTable` field, which keeps addressing stable across nesting. PostgreSQL-only, fail-closed elsewhere; faithfulness proven against a `pg_dump` baseline.
- **Case-insensitive text** — a portable facet `t.text({ caseSensitive: false })` rather than a distinct type, so each engine owns its lowering (PostgreSQL `citext`, SQLite `COLLATE NOCASE`, MySQL default). Additive on the wire; drift reconstructs it from a live `citext` column.
- **Empty-container defaults** — `.default({})` / `.default([])` render `'{}'`/`'[]'` on JSON (and `'{}'::text[]` on text arrays) via a closed empty-only default variant; non-empty containers are rejected at record time.
- **Deferrable foreign keys** — additive `deferrable` / `initiallyDeferred` flags on the FK constraint; renders `DEFERRABLE [INITIALLY DEFERRED]` on PostgreSQL/SQLite and omits it on MySQL (InnoDB is always immediate). Validation enforces that initially-deferred implies deferrable.
- **jsonb value defaults** — arbitrary JSON defaults via a canonical value type with sorted object keys for deterministic checksums; integers only in v1, since float canonicalization risks cross-implementation checksum divergence.
- **Sequences & `nextval()`** — a closed named `nextval(name, {schema?})` reference (never raw SQL), rendered with the `::regclass` cast to match `pg_dump`; PostgreSQL-vendor, fail-closed on SQLite/MySQL, with drift reconstructing the sequence name from the default text.

---

## 10. Invariants

These hold across the engine:

- Migration identity is UUIDv7, immutable, and checksummed. Editing a submitted migration is detected and aborts.
- The journal is append-only and immutability-triggered; net state is the latest event per version.
- Apply is serialized by a project advisory lock and is crash-safe (transactional, or two-phase with idempotent recovery).
- Security is two layers: parse-time deny-list and a least-privilege database role. The creator path can only ever run confined; Platform/Trusted require an operator capability token that the creator path cannot mint.
- The IR is dialect-neutral and closed; there is no raw SQL in expression position, and the three raw islands carry checksummed reasons and a ratcheted budget.
- One IR renders on three dialects through one backend trait; unsupported constructs fail closed, never silently skip.

---

*The API reference for authors is `docs/reference/zeroship-migrate-guide.md`. Intentional Postgres↔SQLite divergences are catalogued in `docs/reference/sqlite-divergences.md`.*
