# Native Rust ORM schema declarations

**Status: Proposed; implementation pending.**

Rust applications should declare their ORM schema in Rust and build with Cargo.
The macro compiles those declarations into typed query APIs and native metadata.
Creator applications continue supplying `schema.runtime.json`; the host decodes
that artifact into the same metadata representation before exposing the ORM.

## Problem

The current [`schema!` macro](../../crates/zeroship-data-macros/src/schema.rs)
reads a migration runtime descriptor during compilation. It generates entities,
columns, relation selectors, and write capabilities, then embeds serialized
field definitions. Generated `Entity::schema()` parses those definitions on
first use. A running Rust service does not need the file, but building its
models requires the creator artifact pipeline.

[`Database::from_schema`](../../crates/zeroship-data-orm/src/orm.rs) already
accepts native field maps. However, `Database::entity` compares installed maps
with generated maps by exact value equality. Changing only the macro syntax
would leave descriptor interpretation and representational mismatches in place.

The change therefore includes native authoring and canonical runtime metadata.
The existing [ORM architecture](../architecture/data-orm.md) and
[data authority boundaries](../architecture/data-system.md) continue to apply.

## Design

```text
Native Rust application                  Creator application
-----------------------                  -------------------
Rust schema! declarations                Migration/build pipeline
           |                                        |
       Rust macros                           schema.runtime.json
           |                                        |
           +--> typed columns / relations            |
           |                                        |
     native definitions                       artifact decoder
           |                                        |
           +-------------------+--------------------+
                               |
                  normalize + validate in the ORM
                               |
                   immutable schema registration
                               |
                    shared query/write pipeline
                  generators + protection + routing
                               |
                     SQL compiler + executor
                               |
                         plain driver
                          /       \
                  PostgreSQL    file SQLite
```

Rust declarations and creator artifacts are alternative sources for a database
handle's metadata. The host chooses the source. Registration never merges a
caller-supplied declaration over an installed descriptor to weaken its rules.

### Rust authoring

Proposed syntax:

```rust,ignore
use zeroship_data_orm::orm::*;

schema! {
    pub models {
        authors {
            #[orm(primary_key)]
            id: Text,
            name: Text,
        }

        posts {
            #[orm(primary_key)]
            id: Text,
            title: Text,

            #[orm(references(authors::id), relation(author))]
            author_id: Nullable<Text>,

            #[orm(assign(on = insert, by = now))]
            created_at: Timestamp,
        }
    }
}

#[derive(FromRow)]
#[orm(entity = models::posts)]
struct Post {
    title: String,
}

#[derive(FromRow)]
#[orm(entity = models::authors)]
struct Author {
    name: String,
}
```

`Text`, `Timestamp`, and `Nullable` describe logical column contracts. Rust
result and input types use the existing native codecs, including typed IDs.
The schema does not require a full-row result struct or Serde derives.

The macro emits the existing entity, column, and relation shapes:

- `models::posts::Entity` implements `Entity` with native collection metadata.
- `models::posts::title` constructs typed column expressions.
- `models::posts::relations::author` identifies the declared reference edge.
- Column capabilities constrain reads, predicates, inserts, and changesets.
- `models::schema()` supplies the declared collections for registration.

Proposed setup and query:

```rust,ignore
let db = Database::connect(binding, options, models::schema()).await?;
let posts = db.entity::<models::posts::Entity>()?;

let rows: Vec<(Post, Option<Author>)> = posts
    .query()
    .with_related(models::posts::relations::author)
    .all::<Post, Author>()
    .await?;
```

The existing relation loader resolves `author_id` from the named edge, even
though the `Post` projection omits it. `FromRow`, `Insertable`, and `Changeset`
remain independent mappings; borrowed inputs, omitted updates, explicit nulls,
and database defaults retain their existing meanings.

### Canonical metadata

`zeroship_data_orm::schema` owns explicit definitions such as `Schema`,
`CollectionSchema`, `ColumnSchema`, and `RelationSchema`. These carry logical
types, nullability, defaults, access capabilities, reference targets, assignment
generators, lifecycle roles, and protection configuration.

The Rust macro emits native constructors. The artifact decoder translates
descriptor fields into the same constructors. Registration normalizes equivalent
type spellings and omitted defaults where their meanings are identical, validates
the collection graph, and publishes the complete schema atomically.

Canonicalization preserves semantic distinctions. Fields that share storage
types may still have different generators, ID contracts, access capabilities,
or protection rules. Relation targets, nullability, and protection configuration
participate in entity compatibility checks. Operational binding identity and
physical catalog evidence remain separate from model definitions.

`Entity::schema()` returns native collection metadata. Binding a typed entity
compares its canonical definition against the host's installed definition.
Failure identifies the collection and differing contract without exposing
configuration values. A typed handle cannot replace that definition.

Queries capture immutable metadata through the existing preparation boundary.
Schema replacement creates a new validated registration, preserves existing
invalidation checks, and does not mutate metadata held by an in-flight operation.
CRUD, relations, assignments, protection, and SQL preparation consume native
metadata directly. The completed cutover removes parallel field-map readers
from these runtime paths; JSON remains at artifact and explicit value boundaries.

### Validation and schema rules

The macro checks declaration syntax, local naming conflicts, and declarations
it can resolve during expansion. Generated traits enforce column types and
capabilities in Rust expressions. The ORM owns authoritative registration
validation for every frontend, including cross-collection relations and
protection/assignment combinations. Shared contract fixtures check that macro
diagnostics and runtime acceptance do not diverge; semantic rules do not grow
into independent frontend implementations.

Every collection explicitly declares a required, non-null `id` as its sole
primary key. The macro neither creates a missing ID column nor supplies an ID
generator. Read projections and aggregate results may omit it.

Other field names carry no implicit behavior. Assignment generators and
lifecycle roles are declared together and validated for consistency. The name
`created_at` in the example is ordinary; its declared generator performs the
assignment. Encryption and masking use the existing shared protection pipeline,
with immutable policy and project keys supplied by the host.

Named forward relations retain the existing loading contract. Their source
column, target column, logical name, and type compatibility are validated.
Adding an authoring frontend does not introduce another relation graph or loading
strategy. Schema-visible framework tables follow ordinary ORM rules; this change
adds no table-name exclusions or privilege to database bindings.

## Migrations and schema authority

Registering metadata describes how the ORM uses tables; it does not create or
alter them. The migration engine continues to own DDL, schema differencing,
journals, and application. The existing migration DSL remains the authoring
source for physical schema changes, including platform migrations.

A Rust service therefore maintains Rust mapping declarations alongside its
migrations. A schema change updates both when their contracts change.
Integration tests apply those migrations and exercise the Rust mappings against
the resulting database. Canonical entity comparison checks the installed ORM
contract; it is not evidence that a physical migration has been applied.

Rust compilation needs neither a database connection nor a generated runtime
descriptor. Existing catalog and protection-floor checks remain on runtime
execution paths. Creator metadata continues to come from its deployment artifact
through the trusted host. Connection authority and DDL permissions do not change.

This proposal does not add automatic schema synchronization, a Rust migration
DSL, or changes to the migration engine. Optional generation of Rust declarations
from migrations or a live database can be considered separately.

## Crate responsibilities

| Crate | Responsibility after the change |
| --- | --- |
| `zeroship-data-macros` | Parse Rust declarations and model derives; emit native metadata constructors, typed columns, capabilities, and relation selectors. |
| `zeroship-data-orm` | Own metadata types, artifact decoding, normalization, validation, registration, and shared query execution. |
| `zeroship-data-v8` | Adapt JavaScript calls/results and compose the host's schema installation; delegate metadata interpretation to the ORM. |
| `zeroship-migrate-*` | Own physical schema evolution and produce creator artifacts under the existing contract. |

No new crate is required. The macro crate does not depend on the ORM crate;
its generated code refers to ORM types at the expansion site. Metadata modules
perform no database I/O. Drivers continue to accept physical statements and
native values without learning about collections, generators, or protection.

## Comparison and tradeoffs

| Reference | What informs this proposal |
| --- | --- |
| [Diesel](https://diesel.rs/guides/schema-in-depth.html) | Rust table declarations generate typed table/column machinery. Its database-driven generation is useful tooling, while declarations remain Rust source. This fits our separation of schema and result mappings. |
| [SeaORM](https://www.sea-ql.org/SeaORM/docs/generate-entity/entity-first/) | Handwritten entities can supply schema metadata. Its entity-first workflow also offers schema synchronization; our migration boundary stays explicit. |
| [SQLAlchemy](https://docs.sqlalchemy.org/en/20/tutorial/metadata.html) | Explicit tables and declarative models feed shared metadata. We adopt a shared runtime representation for independently authored inputs. |
| [Prisma](https://docs.prisma.io/docs/cli/generate) | A separate schema and generated client are a deliberate workflow. We retain the creator artifact path while making native Rust builds independent of it. |
| [SQLx](https://docs.rs/sqlx/latest/sqlx/macro.query.html) | Checked query macros use a live database or prepared offline metadata. Our typed builder instead checks expressions against Rust declarations; integration tests verify agreement with migrations. |

The selected table DSL keeps logical schema definitions separate from Rust
projections and write inputs. An entity-struct derive is a reasonable alternative,
but introducing both authoring styles would expand the surface without solving
another current requirement. Native declarations improve Rust build ergonomics;
they do not remove the obligation to keep mappings and migrations aligned.

## Implementation and verification

Implement in reviewable commits, starting each behavior change with a failing
test and recording its passing result after the fix:

- Establish canonical metadata with Rust/artifact equivalence cases and genuine
  mismatch cases. Cover type defaults, reference edges, capabilities, generators,
  and protection. Test atomic rejection of an invalid registration.
- Add the native macro frontend with compiler pass/fail coverage. Exercise
  missing or invalid identity, duplicate names, unresolved references, nullable
  columns, generated write restrictions, and independent read/write mappings.
- Change `Entity` and database registration to native metadata. Route runtime
  consumers through that representation and remove embedded JSON-string parsing.
  Preserve metadata invalidation and captured transaction behavior.
- Convert every Rust macro caller, compiler fixture, example, and reference
  document. Remove the file-based macro form directly, with no compatibility
  alias or fallback. Creator artifact decoding remains a supported input.
- Apply migration fixtures through the existing engine and run mandatory
  PostgreSQL Testcontainers and file-backed SQLite tests. Verify generators,
  protection, named relation projections, typed IDs, native binary/integer
  values, and transaction execution using native declarations.
- Run V8/SDK and artifact contract checks to verify that decoding into native
  metadata preserves creator behavior. Review for residual JSON field-map
  interpretation, duplicated semantic validation, and field-name conventions.

Completion requires a Rust consumer whose models compile without a runtime
descriptor file or database connection, equivalent behavior from Rust and
creator metadata, and passing live-database coverage. Source citations and code
examples in this proposal are design context; the implementation's tests and
reference documentation become the executable and user-facing contracts.
