# Dual-Dialect, Structured-Max SQL-Features DSL — comprehensive design

> Status: design proposal (UNTRACKED — do not commit). Worktree
> `appbase-migrate`, branch `feat/db-migration-engine`. Synthesizes seven
> per-group designs for the `zeroship-migrate` engine onto two axes:
> **AXIS 1 (structure)** — closed typed IR + closed `Expr` AST first, raw SQL
> only as a justified, capability-gated, deny-list-scanned last resort; and
> **AXIS 2 (dialect)** — maximize the cross-dialect CORE (per-dialect render
> for everything BOTH Postgres and SQLite support), reserve PgOnly for the
> genuinely PG-specific, fail-closed (never silent-skip) where SQLite cannot.

## Thesis

A migration tool for `zeroship` is **general-purpose and dual-dialect**. The
guiding rules:

1. **Structured-max / raw-min.** Default to a first-class closed IR `Op` + fluent
   DSL + structured render via the closed `Expr` AST. Raw SQL is a last resort,
   confined to genuinely unstructurable islands (an arbitrary `SELECT` body, a
   PL/pgSQL body), always operator-capability-gated and `pg_query` deny-list
   scanned. Even there, design the maximal structured wrapper and a closed subset
   where feasible (closed `SelectStmt` for views; `TriggerGuardBody` for the
   tamper-guard function family).
2. **Cross-dialect-core-max.** For every construct, state PG support AND SQLite
   support, then classify: `cross-dialect-core` (both → CORE op, per-dialect
   render), `dialect-rendered PG-first + SQLite story` (emulate where sound, else
   fail-close), or `PG-vendor` (genuinely PG-specific → PgOnly, capability-gated,
   fail-closed on SQLite). Never fail-close on SQLite for anything SQLite supports.
3. **Two orthogonal gates.** The *dialect* axis decides render vs emulate vs
   fail-close. The *trust* axis (capabilities) decides whether an author may use
   the op at all. A dialect reclassification never weakens a trust gate.

## Master table

| Construct | Dialect class | Structure | PG render / SQLite story | Capability |
|---|---|---|---|---|
| CREATE/DROP/REPLACE VIEW | cross-dialect-core | structured (`ViewBody::Select`) + RawSql escape | PG native `OR REPLACE`; SQLite DROP+CREATE | none / RawSql for raw body |
| CREATE MATERIALIZED VIEW, REFRESH | PG-first / PG-only | structured + RawSql escape | PG native; SQLite fail-close (no analogue) | none (PgOnly core) |
| WITH CHECK OPTION | PG-first | structured (closed enum) | PG clause; SQLite fail-close (read-only views) | none |
| INSTEAD OF trigger (redirect) | cross-dialect | structured (closed DML actions) | SQLite inline body; PG synthesized plpgsql fn | Trigger (+Function on PG) |
| CREATE/DROP TRIGGER (`body`) | cross-dialect-core | structured (closed stmt list) | SQLite `BEGIN…END`; PG synthesized fn | none |
| CREATE/DROP TRIGGER (`executeFunction`) | PG-vendor | structured (names a fn island) | PG `EXECUTE FUNCTION`; SQLite fail-close | Trigger |
| GENERATED ALWAYS AS (expr) | cross-dialect-core | structured (closed `Expr`) | both STORED; SQLite also VIRTUAL; VIRTUAL on PG fail-close | none |
| IDENTITY / serial sugar | cross-dialect-core | structured | PG `AS IDENTITY`; SQLite `INTEGER PK AUTOINCREMENT` iff sole int PK, else fail-close | none |
| Column COLLATE | core (Binary) / PG-first (Named) | structured (validated ident) | PG `COLLATE`; SQLite builtins or fail-close | none |
| CREATE/DROP SCHEMA | PG-first + SQLite story | structured | PG native; SQLite project-ns no-op, foreign fail-close | Schema |
| CREATE/ALTER/DROP DOMAIN | PG-first + SQLite emulation | structured (closed `Expr`+`DomainValue`) | PG `CREATE DOMAIN`; SQLite base type + inlined CHECK at use site | Type |
| CREATE TYPE AS ENUM / ADD VALUE | PG-first + SQLite emulation | structured (string literals) | PG native; SQLite TEXT + `CHECK (col IN …)` | Type |
| CREATE TYPE composite / RANGE | PG-first; SQLite fail-close | structured | PG native; SQLite no sound emulation | Type |
| CREATE/ALTER/DROP SEQUENCE | PG-first; SQLite fail-close | structured (`IrScalar` bounds) | PG native; SQLite fail-close (standalone seq) | Sequence |
| DEFAULT nextval | PG-first + SQLite story | structured (closed `IrDefault::Nextval`) | PG `nextval`; SQLite AUTOINCREMENT iff sole int PK else fail-close | (pg) |
| Multi-col FK / arbitrary refcols | cross-dialect-core | structured (lift restriction) | both | none |
| CHECK | cross-dialect-core | structured (closed `Expr`) | both | none |
| Deferrability (FK) | cross-dialect-core | structured (closed enum) | both | none |
| Deferrability (UNIQUE/PK) | PG-first; SQLite fail-close | structured | PG clause; SQLite fail-close | none |
| NOT VALID + VALIDATE | PG-first + SQLite emulation | structured | PG two-step; SQLite rebuild validates (end-state identical) | none |
| EXCLUSION constraint | PG-vendor | structured (closed op-set) | PG `EXCLUDE USING`; SQLite fail-close | Exclusion |
| INHERITS | PG-vendor | structured (conditional-vendor field) | PG `INHERITS`; SQLite fail-close | Inherits |
| COMMENT ON | PG-vendor | structured (escaped literal) | PG `COMMENT ON`; SQLite fail-close (rec.) / shadow opt-in | Comment |
| Roles / membership / default-priv | PG-vendor | structured | PG native; SQLite fail-close | Role / Grant |
| GRANT/REVOKE (incl. column) | PG-vendor | structured (closed `Privilege`/`GrantTarget`) | PG native; SQLite fail-close | Grant |
| RLS enable/force; policies | PG-vendor | structured (closed `Expr` predicate) | PG native; SQLite fail-close | Rls / Policy |
| CREATE/DROP FUNCTION | PG-vendor | structured wrapper + raw body (or `Guard` subset) | PG native; SQLite fail-close | Function |
| CREATE/DROP EXTENSION | PG-vendor | structured (allowlist-gated) | PG native; SQLite fail-close | Extension |
| Partitioning (parent/child/attach/detach) | PG-vendor | structured (closed bounds) | PG native; SQLite fail-close (no sound emulation) | Partition |
| FDW / publications / event triggers / rules / operators / aggregates / casts / TS-config / tablespaces | raw-only or deferred or DENY | — | `pg.sql` (scanned) or hard-deny; `ALTER SYSTEM` always denied | RawSql |

See the seven per-group sections (verbatim in the orchestration brief) for full
IR shapes, sub-enums, DSL methods, validators, and SQLite emulation decisions.

## New capabilities

Existing: `Extension, Schema, Role, Grant, Rls, Policy, Trigger, Function, RawSql`.
Added by this spec: `Type` (domains/enums/composites/ranges), `Sequence`,
`Exclusion`, `Comment`, `Inherits`, `Partition`. `confined()` grants none of the
privileged set; `operator()` grants all; `local()` grants structural DDL but not
`Role`/`RawSql`.

## Phased plan

1. Closed `SelectStmt` (`select.rs`) + `Expr` extensions (qualifier, aggregates,
   subqueries) + context-gated validator; views/matviews.
2. Triggers reclassification (`TriggerAction`/`TriggerStmt`, `RowQualifier`);
   INSTEAD OF redirect.
3. Column facets: `IrGenerated`, `IrIdentity`/`SafeI64`, `IrCollation`,
   serial/identity sugar, `IrDefault::Nextval`, sequences.
4. Schemas/types/domains/enums + the migration TYPE REGISTRY + `ColType::Named`,
   `Expr::DomainValue`.
5. Advanced constraints + COMMENT (deferrability default-flip, NOT VALID/VALIDATE,
   multi-col FK lift, EXCLUSION, INHERITS).
6. Functions (`FuncBody::Guard` subset), extensions, partitioning, the long-tail
   raw audit.

Each phase bumps `CURRENT_IR_VERSION` (checksum-neutral via omitted-when-absent),
regenerates `op-ir.schema.json` + `sdks/migrate/src/generated/ir.ts`, threads
exhaustive `Op` match arms, and ships RED-first regression tests (PG render
goldens, SQLite emulate/fail-close, Confined `VENDOR_OP_DENIED`).
