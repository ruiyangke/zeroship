# Shared ORM SQL compilation

**Status: Compiler redesign in progress. Crate consolidation is implemented and validated.**

Consolidate runtime SQL construction around a shared statement representation,
dialect compilation, and ORM-owned execution strategies. Rust and TypeScript
retain the same collection semantics. PostgreSQL remains the production backend;
file-backed SQLite remains the local development backend.

## Scope and constraints

Merge `zeroship-data-sql` into `zeroship-data-orm::sql`. Keep the macro, V8
adapter, CDC wire, and CDC server crates separate. The shared physical schema
identity moves to `zeroship-core`; the migration service changes its dependency
and import to that owner. Migration DDL and execution behavior, the deploy
artifact, connection pooling, the transaction reducer, and the separate feature
roadmap remain outside this change. Preserve the native value path, required
`id` contract, descriptor-driven generators, fixed application mask policy, and
project-owned encryption keys.

The physical driver continues to acquire sessions, bind parameters, execute,
cancel, and settle. It acquires no collection, policy, search, or upsert API.
Database verification remains mandatory, using owned PostgreSQL containers and
explicit SQLite files. No feature flag or missing-service skip is introduced.

## Current implementation and the problem

These are observations of the current code, not descriptions of the proposal:

| Current path | Consequence |
| --- | --- |
| [Runtime CRUD compilation](../../crates/zeroship-data-orm/src/sql/compile.rs) constructs statements from native records. [Typed plan rendering](../../crates/zeroship-data-orm/src/sql/render/postgres.rs) separately renders reads and writes. | Expressions, statement construction, and compiler guarantees have overlapping implementations. |
| [Rust filters](../../crates/zeroship-data-orm/src/orm/model.rs) first become dynamic native records; [filter decoding](../../crates/zeroship-data-orm/src/sql/filter.rs) subsequently constructs predicates. | Rust pays for an intermediate filter vocabulary even when the input was typed. This is allocation and conversion, not JSON text serialization. |
| Runtime `SqlDialect` dispatch currently covers PostgreSQL and SQLite. | Downstream compiler registration still requires replacing this closed dispatch. |
| Ordinary find compilation interpolates validated pagination values; [relational compilation](../../crates/zeroship-data-orm/src/sql/compile/read.rs) binds them. | Statement shape varies differently between paths. |
| [Determinism tests](../../crates/zeroship-data-orm/tests/sql/determinism.rs) exercise the standalone renderer. | They do not establish the same property for runtime CRUD compilation. |

Keep the existing strengths: native parameters, validated identifiers, mandatory
database tests, source-aware projections, and the encrypted-upsert identity guard.
String appending inside a compiler is a normal implementation technique. The
design problem is who owns syntax and which validated representation reaches it.

## Comparison with established ORMs

| Reference | Observed approach | Adopt here | Adaptation for zeroship |
| --- | --- | --- | --- |
| [Diesel AST passes](https://docs.diesel.rs/master/diesel/query_builder/struct.AstPass.html) and [query fragments](https://docs.rs/diesel/latest/diesel/query_builder/trait.QueryFragment.html) | Query nodes walk a backend-specific compiler; identifiers and binds use distinct operations. Cache eligibility is considered during compilation. | Structured nodes and a centralized SQL writer. | Rust field types remain checked at the API. The internal statement is a runtime Rust value shared with V8, so callers do not acquire a backend type parameter. |
| [SQLAlchemy compilation](https://docs.sqlalchemy.org/en/20/core/compiler.html) and [ORM upserts](https://docs.sqlalchemy.org/en/20/orm/queryguide/dml.html#orm-upsert-statements) | Expression objects have dialect-specific compilation rules; upsert constructs expose backend differences. | Explicit compilation and unsupported-operation errors. | Keep our portable application contract and check whether a backend can preserve it. The physical driver remains independent of the application model. |
| [Prisma database upserts](https://www.prisma.io/docs/orm/v7/reference/prisma-client-reference#database-upserts) | Native upsert selection depends on the operation and connector; other shapes use client orchestration. | Separate the logical operation from its execution strategy. | Keep native upsert for the supported ordinary path and the existing transaction-bound encryption path. Do not add a generic read-then-write fallback. |

These are design lessons, not dependency choices. This proposal adds no Diesel,
SQLAlchemy, Prisma, or SeaQuery runtime dependency.

## Target ownership

```text
Rust typed builders                 TypeScript / V8
        |                                  |
        |                         capture owned input
        +------------------+---------------+
                           |
                   zeroship-data-orm
          logical operation + descriptor resolution
          authority context + generators + protection
          execution strategy + result layout
                           |
                  resolved SQL statement
                           |
                   data-orm::sql
                  expressions + bindings
                    dialect compilation
                     /              \
                PostgreSQL         SQLite
                     \              /
                    SQL + native parameters
                           |
                     ScopedExecutor
                    existing owned session
                           |
                       DriverSession
```

The ORM's existing `PreparedOperation` remains the execution owner. Introduce no
generic query-graph executor or new operation virtual machine. Reuse its operation
payloads and the shared expression grammar; do not add parallel general-purpose
logical and physical query trees. Resolve field references into typed physical
references when constructing the statement.

The ORM owns access decisions, soft-delete visibility, generated assignments,
masking, encryption, and source identity. The SQL compiler sees the resulting
expressions and storage types, without policy objects or encryption keys.
Internal identity projections remain in the ORM result layout until protection
and decoding finish. This does not bypass database grants. An unmatched
outer-joined row keeps its optional-row shape. Catalog protection-floor checks
remain ORM services; resolving a descriptor never substitutes for those checks.

`data-macros` generates field metadata and model codecs. Rust builders construct
typed expressions directly. `data-v8` captures native inputs and invokes the
shared ORM decoder; it does not implement schema resolution or SQL compilation.
Validation remains mandatory for both callers after decoding.

The SQL module remains pure: statements, compilation, and storage codecs do no
I/O and receive no application policy or encryption keys. Moving modules removes
the Cargo dependency fence around that code; it does not change the compiler's
responsibilities. Backend extension contracts remain public, while compiler
implementation helpers stay private.

Native values live in `data-orm::value`, shared by SQL, drivers, models, and V8.
`SchemaName` lives in `zeroship-core::schema_name` with its validation and typed
error. SQL quoting remains with the respective SQL consumers. The migration
service must not acquire an ORM dependency for that identity type.

`data-macros` retains no ORM implementation dependency. `data-v8` depends on the
ORM for native values and operations. The CDC relay continues to share only its
wire crate with the ORM in shipped code.

Crate consolidation moves existing SQL behavior, tests, and benchmarks together
and deletes the old package and every active dependency on it. It precedes the
compiler redesign below; moving code alone does not consolidate its renderers.

The crate consolidation passed `cargo xtask test data`, the core schema-identity
tests and documentation contract, migration-service provisioning tests, CLI and
ORM benchmark compilation, and DB SDK/bootstrap tests and type checks. The SQL
integration tests now run from the ORM package; native values and their macros
use the ORM public path.

Proposed module ownership:

| Location | Responsibility |
| --- | --- |
| `data-orm::orm` and `crud` | Operation preparation, descriptor resolution, result layouts, and write orchestration. |
| `data-orm::sql::statement` | Consolidated statement nodes and typed physical references, replacing overlapping plan and collection-builder representations. |
| `data-orm::sql::compiler` | Shared writer and dialect implementations, consolidating the current `compile` and `render` paths. |
| `data-orm::sql::codecs` | Shared logical validation and registered SQL storage codecs. |
| `data-orm::backend` | Registration, live support checks, scoped execution, and identity-allocation strategies. |

These are target module names. Retained identifiers, temporal validation, catalog
metadata, and protection codecs need no unrelated renaming.

## Statement and value representation

Consolidate the existing `DbPlan`, statement nodes, and runtime operation builders
into a resolved statement family. Reuse the current predicates and validated
identifiers. Reads cover ordinary and joined projections, grouping, aggregates,
sorting, and pagination. Writes cover inserts, assignments, deletes, and upserts.
Array operations remain structured assignment expressions.

The statement grammar has no caller-supplied SQL text node. An expression names a
column, a native bind, SQL null/default, arithmetic, a supported function, or an
incoming upsert value. Column references carry source identity and storage type.
Absent input, SQL NULL, and a database default remain distinct.

Constructors check source membership, expression types, legal clause placement,
and resource budgets. `Incoming` is valid only in its insert's conflict-update
context. Ordinary query expressions cannot refer to internal projection slots.
SQL NULL tests are predicate nodes; JSON null remains a JSON value. Storage type
is explicit for a null or an empty typed container and is never inferred from a
column name. Ordinary bulk mutations retain their unbounded matching semantics;
per-row encrypted mutations retain their existing caps. Read pagination limits
do not become write row caps.

The compiler returns `CompiledQuery`, replacing `BuiltQuery` and `RenderedSql`:
SQL text with ordered native parameters. Its public inspection surface exposes
SQL and parameter types/count without including values in Debug or tracing by
default. Actual parameters remain accessible to the trusted execution adapter.
The ORM keeps the result layout and expected outcome (rows or affected count)
beside the compiled query. Compilation consumes owned statement values so that
parameter collection can move buffers instead of cloning them. Retries retain
the original write input only when the chosen strategy needs it.

Use a compiler-internal writer for syntax, identifiers, and binds. It owns bind
numbering and parameter limits. Bind values never become SQL fragments. Bind
pagination values consistently. Normalize unordered assignment input where doing
so preserves semantics; retain join order, projection order, and sort order.
Structural ordering must not depend on bound values. Do not sort arbitrary
expressions or deduplicate volatile operations to improve cache hits.

## Dialect registration and codecs

The host registers an immutable SQL dialect bundle alongside the scoped executor.
It contains a compiler, storage codecs, and a description of implemented SQL
features. Built-in implementations live in `data-orm::sql`; runtime probing and
database I/O stay in the ORM backend adapter. Shared configuration is thread-safe;
sessions and execution remain local to their compio thread.

The proposed extension surface separates pure work from execution:

| Contract | Input and result | Owner |
| --- | --- | --- |
| `SqlCompiler::check` | Statement requirements and effective SQL support; returns a structural/capability error or acceptance. | Pure `data-orm::sql` implementation. |
| `SqlCompiler::compile` | Owned resolved statement and effective SQL support; returns `CompiledQuery` or a typed error. Revalidates the actual statement. | Pure `data-orm::sql` implementation. |
| SQL storage codec | Storage type and owned value; returns an encoded or decoded native value. | Pure `data-orm::sql` implementation. |
| Identity allocation service | Resolved allocation request and the operation's scoped session access; returns native IDs or a typed error. | ORM backend implementation with local async I/O. |

These contracts have no success-by-default implementation. Requirement extraction
includes retry statements, returning fields, generated assignments, and identity
allocation prerequisites. Preflight these before allocating IDs or issuing a
write reservation. Value-dependent failures still use the atomic write frame;
they do not imply that a database sequence increment can be rolled back.

The registration flow is part of the contract:

```text
factory configuration: compiler + codecs + required server features
        |
capture app, descriptor, actor, transaction scope, registration identity
        |
open/bind backend and validate actual server support
        |
verify captured registration and current transaction session match
        |
resolve physical namespace and compile for this bound registration
```

Connection setup may perform support probes. Unsupported application operations
fail before application mutation, not necessarily before any database I/O. Server
support cannot exceed what the selected compiler implements. Requirements are
checked again by compilation; a feature advertisement alone is not validation.

The opaque registration identity includes compiler/codec configuration as well
as connection configuration. Retain it in the captured route and verify it on
backend binding and session use. Dialect display names are not identities. Do not
re-read the current global backend after a request yields or reuse SQL prepared
for a replacement registration. Tests must cover asynchronous initialization and
expired transaction handles as well as ordinary execution.

Replace the closed runtime `SqlDialect` dispatch with this registration. Partial
MySQL runtime compilation disappears; migration-engine MySQL support is outside
scope. New SQL backends implement the shared statement contract and pass its
conformance suite. Adding a driver alone is insufficient. A downstream test
implementation must be registerable without editing a central vendor enum. The
statement grammar remains intentionally bounded; new SQL language features still
require explicit grammar and validation work.

Storage codecs must move with the compiler cutover. The current boolean, JSON,
temporal, decimal, vector, and geographic encodings cannot remain hidden switches
on the old enum. Shared logical validation precedes storage lowering. Dialect
codecs work with storage types and native values; drivers perform wire or SQLite
binding. JSON encoding remains specific to JSON storage and explicit wire formats.

The ordering is explicit: validate logical values and apply assignments; preserve
the plaintext needed for masks; encrypt using the resolved identity; encode the
resulting storage values; then place them in descriptor-selected physical columns.
Reading restores storage values before the ORM's decrypt/mask/unmask passes and
final projection. Ciphertext is bound as bytes regardless of its logical field
type. Preserve encoded JSON strings, temporal precision, exact decimals, and
numeric identities through this sequence. Normal writes cannot traverse a codec
again merely because SQL rendering moved to another module.

SQL support includes explicit conflict targets, conditional conflict updates,
returning projections, generated-identity insertion, and bind limits. The compiler
must refuse a requested combination it cannot render correctly. Capability
declarations do not grant application authority. SQL syntax support also does not
prove that a backend's locking and isolation behavior supports an ORM strategy.

Database-generated identity allocation is an ORM backend service above the plain
driver. Move the existing sequence and SQLite writer-reservation implementations
behind that service; route all of their commands through the same scoped session.
It receives a resolved table, identity storage type, and allocation request, with
no access to encryption keys. It returns native identities or an explicit refusal.
The compiler owns the SQL spellings for reservation and allocation statements;
the service owns their order, returned-value checks, overflow checks, and session
requirements. Preserve sequence gaps and rollback behavior instead of promising
gap-free identifiers. Internal SQL support need not become a creator operation.

## Upsert contract

The following is a representation sketch, not a new public SDK surface:

```rust,ignore
Upsert {
    insert: Insert,
    conflict_target: ConflictTarget,
    update: Vec<Assignment>,
    update_condition: Option<Predicate>,
    returning: Projection,
}
```

`Incoming(column)` refers to the proposed insert value. `Current(column)` refers
to the conflicting row. Dialect compilers choose their SQL spellings. The ORM
constructs insert-only and update-time generator expressions from the descriptor.
It never assigns the existing row's `id` from the incoming row.

The operation targets the requested unique key, preserves existing identity and
insert-only fields, applies update assignments atomically, and decodes the
statement's returned row through the normal result pipeline. Composite conflict
targets keep their grouping. Constraints outside the selected conflict action
remain enforced; the compiler cannot broaden that action to another unique key.
The returned projection retains the supported database's statement semantics;
this does not promise a fresh read after arbitrary user triggers.

Conflict targets are nonempty lists of declared, supplied, application-owned,
unencrypted columns, with no repeated column. Reuse the existing runtime validation;
do not invent a unique-index proof from a field-name list. The current field maps
are not a complete index catalog. Database constraint inference remains
authoritative, and an invalid target must fail atomically. Partial or expression
conflict-target syntax is outside the current application API.

Candidate identity lookup must agree with the conflict target's equality,
including nullable keys. A null-aware filter is not automatically equivalent to
a uniqueness conflict. The backend strategy must establish lookup equivalence
before using a probe. If it cannot, refuse the protected-write strategy before
identity allocation; ordinary native upsert retains database semantics. Test
nullable keys explicitly and never select an arbitrary matching row as identity.

An update without application-owned changed fields retains the existing
row-returning behavior. Represent the required self-assignment explicitly and
test its effects; substituting `DO NOTHING` would change returned rows and
database-trigger behavior. Generator assignments still come from the descriptor.

This distinction is load-bearing: MySQL's native upsert can match any unique key,
while PostgreSQL and SQLite support a specified conflict target. A syntax rewrite
alone does not establish equivalent behavior. [MySQL conflict behavior](https://dev.mysql.com/doc/refman/8.4/en/insert-on-duplicate.html),
[Diesel conflict targets](https://docs.diesel.rs/main/diesel/helper_types/type.OnConflict.html).

Ordinary supported upsert executes the native statement. Encrypted upsert keeps
the ORM's guarded strategy:

```text
validate operation and required SQL support
        |
open atomic write frame (transaction or nested savepoint)
        |
resolve candidate identity / allocate declared identity if needed
        |
prepare ciphertext using that identity
        |
execute upsert with Current(id) = Bind(expected_id)
        |
row returned --------------------> decode and settle
        |
guard skipped conflicting row
        |
resolve winning identity and retry with original plaintext
```

Retain the existing retry bound and rollback behavior. The empty-row branch is
interpreted only by the ORM strategy that installed the identity guard. It is
not a generic retry rule for successful writes or an interpretation of database
errors. The compiler renders the guard as an ordinary predicate; it has no
encryption-specific operation.

PostgreSQL's conditional conflict update locks a conflicting row even when the
condition prevents the update. Under Read Committed, the next lookup can observe
the committed winner. A transaction with a fixed snapshot can instead fail with
a serialization error; preserve that error and let the existing transaction
protocol handle it. A savepoint does not refresh the outer transaction's snapshot.
[PostgreSQL conflict updates](https://www.postgresql.org/docs/current/sql-insert.html),
[transaction isolation](https://www.postgresql.org/docs/current/transaction-iso.html).

SQLite keeps the writer reservation across identity allocation, mutation, and
settlement. Its strategy is verified independently of PostgreSQL's tuple-lock
behavior. No connection replacement, transaction restart, or network-error retry
may happen inside the identity retry loop. Cancellation and ambiguous settlement
retain the existing reducer's outcome rules.

A skipped guard must not publish a committed row change or apply update
generators. Publish mutations through the existing commit-aware effects path.
Preserve usage-metering semantics separately from affected-row and CDC semantics.
Unmask audit persistence keeps its existing separation from creator rollback.

## Caching and measurement

Statement stability and compilation caching are separate concerns. The existing
PostgreSQL driver owns its bounded prepared-statement cache. This refactor does
not add an ORM compilation cache. SQLAlchemy's cache avoids repeated compilation
of compatible expression structures; it is additional infrastructure that needs
its own cacheability and invalidation contract. [SQLAlchemy caching](https://docs.sqlalchemy.org/en/20/faq/performance.html).

Extend the existing `bench_query_build` benchmark to cover the shared production
compiler, Rust input construction, and SDK filter decoding. Measure allocations,
compilation work, and statement reuse across equivalent inputs. Keep results in
benchmark output rather than prose. Do not infer a performance improvement from
removing code or from a standalone renderer benchmark.

Native collection builders and typed-plan rendering now return
`sql::compiler::CompiledQuery`. Its Debug output reports SQL and native parameter
types; execution can borrow bindings or consume the output to transfer its
buffers. Typed-plan database tests use the production native parameter encoder.
Typed-plan rendering uses the shared writer for identifier quoting, native bind
allocation, and statement-wide parameter limits. Collection builders still own
their parameter collection until their operation cutovers.

## Implementation checklist

- [x] Consolidate the SQL crate into the ORM and validate its consumers.
- [x] Close incomplete runtime dialect admission; retain migration dialect support.
- [ ] Consolidate compiler output and native binding through a shared writer.
- [ ] Register compiler, storage codecs, effective support, and immutable identity together.
- [ ] Capture registration and verify it against bound backends and scoped sessions.
- [ ] Replace production upsert construction with a resolved statement and capability preflight.
- [ ] Move generated-identity allocation behind the scoped ORM backend service.
- [ ] Verify protected upsert concurrency, nullable targets, no-change updates, and effects.
- [ ] Cut over other writes and remove their replaced renderers.
- [ ] Cut over ordinary and relational reads, including direct Rust expressions and bound pagination.
- [ ] Cut over search and internal protection SQL and remove remaining runtime enum dispatch.
- [ ] Verify downstream registration and the complete database, SDK, bootstrap, and V8 contracts.
- [ ] Update production-path benchmarks and stable architecture documentation.

## Cutover and TDD

Each operation cutover includes its callers, behavior tests, and deletion of its
replaced renderer. No compatibility aliases or alternate runtime switches remain.

| Cut | RED evidence | GREEN implementation and removal |
| --- | --- | --- |
| Crate consolidation | The workspace still exposes a standalone SQL package; the shared schema identity and ORM SQL entry point are unavailable. | Move shared identity to core, SQL into the ORM, and native values to the ORM value module; update consumers, tests, benchmarks, and documentation; delete the SQL package. |
| Compiler foundation | Unsupported operations fail before mutation; runtime SQL shape and bind ordering expose the current inconsistencies. | Shared writer, statement output, and registered dialect path. |
| Upsert | Production-path tests cover key preservation, guarded conflicts, native value binding, and capability refusal. | Route ordinary and encrypted upserts through the new statement; delete the old upsert renderer. |
| Other writes | Insert/default/null, assignment, bulk count, encrypted atomicity, and rollback cases exercise the actual ORM path. | Move inserts, updates, deletes, and lifecycle operations; remove duplicate write builders. |
| Reads | Ordinary and joined reads agree on predicates, codecs, hidden identity, pagination, and aggregates. | Share resolved SELECT compilation and delete competing read renderers. |
| Remaining SQL | Search and internal protection statements preserve their current behavior and parameter discipline. | Remove remaining runtime dialect switches and update the benchmark to the final path. |

Characterization tests record intentional current semantics; defect regressions
must fail for the intended reason before their fix. Existing PostgreSQL race
tests and file SQLite regressions remain required throughout the cutover.

During a cutover, each migrated operation has its replacement path exclusively.
Unmigrated operations can retain their existing implementation until their own
cut, without a caller-selectable mode or a second renderer for a migrated
operation. Freeze shared writer/value contracts with the upsert cut before moving
the remaining families. The runtime enum disappears only when all of its runtime
callers have moved; incomplete MySQL admission is closed at the foundation cut.

Completion also accounts for SQL outside collection DML:

| SQL family | Final owner and boundary |
| --- | --- |
| Ordinary CRUD, joins, aggregates, lifecycle assignments | Shared statements and dialect compilers; ORM prepares them. |
| Search expressions and array updates | Structured expressions and dialect compilation; ORM search strategies retain extension checks and local algorithms. Relation-loader behavior is outside this compiler refactor. |
| Raw protected-column reads and unmask audit writes | Shared statement writer and parameter discipline; ORM supplies authority, identity, and audit provenance. |
| Generated-identity reservation and allocation | SQL compiler owns syntax; the ORM backend service owns the session-bound sequence. |
| BEGIN, savepoints, role/timeouts, cancellation, catalog inspection, provisioning, backup, and replication commands | Trusted backend/service implementation with native bindings. These are explicit backend-specific infrastructure contracts, not creator query nodes. Existing privilege boundaries remain in force. |

Do not force privileged or infrastructure SQL into the creator statement grammar
to claim that every SQL string disappeared. Completion means runtime collection
compilation has a sole path and dialect-specific infrastructure has an explicit
owner. The proposal adds no privileged capabilities to the worker.

The acceptance cases below are required on the path used by `PreparedOperation`:

| Area | Required evidence |
| --- | --- |
| Binding and syntax | Hostile values stay in parameters; identifiers are quoted; nulls, bytes, exact numbers, timestamps and JSON values retain their types; bind limits include generated assignments and guards. |
| Structural stability | Reordered equivalent input maps and different pagination values preserve supported statement shape; joins and requested ordering retain their meaning. |
| Registration | A downstream implementation can register compiler/codecs; a mismatched or replaced registration cannot use another session; expired transaction handles stay expired. |
| Unsupported shapes | A constrained test backend rejects conditional upsert, returning, or identity allocation before an application mutation; target data and generators remain untouched. |
| Upsert semantics | Insert and conflict paths preserve the expected ID; insert-only fields stay fixed; write generators apply correctly; unrelated uniqueness violations, nullable/composite keys, empty updates, and absent/null/default inputs have explicit outcomes. |
| Protected concurrency | Distinct connections race with encrypted and masked values, generated and supplied identities, and nested transactions. Verify stored ciphertext by subsequent reads; cover guard exhaustion, serialization errors, cancellation, and rollback. |
| Effects and materialization | Hidden IDs remain internal projection slots, joined optional rows retain shape, skipped guards produce no row-change publication, bulk counts avoid unnecessary returning rows, and audit persistence survives creator rollback. |

Use live database orchestration to observe the relevant conflict or lock before
releasing a competitor. Timing sleeps are not evidence that the race occurred.
SQL fixtures supplement these tests; matching statement text alone is insufficient.

Run focused tests during each red-to-green cycle, then the existing
`cargo xtask test data` suite, SDK and bootstrap tests/type checks, and V8 tests
against freshly built SDK artifacts before closing a cutover. Use executable
behavior and compiler contracts to prove boundaries. Source-text counts are not
substitutes for those tests.

## Design review

This is a self-review against the implementation and the primary references
above, not an independent external review. The draft was revised as follows:

| Review finding | Revision | Implementation proof |
| --- | --- | --- |
| A compiler trait left codecs and generated-identity behavior dependent on the vendor enum. | Register codecs with compilation; put session-bound allocation behind the ORM backend service. | Downstream registration and native-type/identity conformance tests. |
| Dialect name equality did not bind compilation to the session that executes it. | Capture immutable registration identity before yielding; validate backend and session identity after binding. | Replacement, async initialization, and transaction-scope regressions. |
| SQL feature flags did not establish conditional-upsert locking or snapshot behavior. | Separate compiler support from ORM strategy eligibility; specify guarded retry, preflight before allocation, and error propagation. | PostgreSQL lock-observed races and isolation failures; SQLite writer-reservation tests. |
| Physical lowering could apply codecs again or expose hidden identities before protection. | Specify codec/protection ordering and retain result provenance in the ORM. | Native value round trips and protected ordinary/joined projections. |
| The draft could imply full index knowledge and interchangeable no-op upsert actions. | Keep database constraint inference authoritative, distinguish nullable-key lookup, and preserve explicit self-assignment behavior. | Invalid/composite/nullable targets and no-change upsert outcomes. |
| Full canonicalization or a new cache could change expression semantics and grow scope. | Limit normalization to safe structural ordering; keep existing driver caching and benchmark production compilation. | Shape, ordering, and allocation measurements on actual operation paths. |
| Moving every SQL string into the query AST would mix infrastructure with creator operations. | Define a family-by-family completion boundary and preserve trusted infrastructure ownership. | Runtime caller cutovers plus existing architecture and authority suites. |

The revised crate decision is to consolidate SQL into the ORM first. The
subsequent compiler redesign starts with upsert, including its dialect, codec,
identity, and result contracts. Adding another database vendor, a new ORM
cache, or a broader expression language requires a separate proposal.
