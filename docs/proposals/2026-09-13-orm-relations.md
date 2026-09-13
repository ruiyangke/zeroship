# Named ORM relations

Status: named forward loading implemented. Further relation shapes remain below.

Queries select logical schema edges without repeating foreign-key definitions.
The source scalar remains available alongside the loaded target.

```ts
author_id: t.text().references("users", "id", { relation: "author" })

const { data: posts } = await db.posts.find().with({ author: true });
posts[0].author_id;
posts[0].author?.name;
```

The runtime field descriptor carries `relation`, `refTarget` and `refColumn`.
The ORM resolves the edge; neither query callers nor the SDK define joins.
Generated Rust metadata exposes `posts::relations::author` for
`.with_related(...).all::<Post, User>()`, returning `(Post, Option<User>)`.

Relation names are explicit, unique within their source collection, and distinct
from its columns. Multiple foreign keys to the same table remain unambiguous.
Names are deployment metadata and do not change database constraints or require
catalog introspection.

The shared loader batches declared forward references, preserves parent
pagination, uses the captured transaction route, records target read dependencies
even for an empty parent result, and applies target protections before returning
rows. Source and target descriptors are checked across asynchronous execution.
The architecture contract is in [data-orm.md](../architecture/data-orm.md).

## Review against established ORMs

[SQLAlchemy](https://docs.sqlalchemy.org/en/20/orm/queryguide/relationships.html)
separates relationship properties from foreign-key columns and supports joined
and select-in loading. We adopt that separation and keep result semantics
independent of the execution strategy. Batching is a starting point, not a claim
that it outperforms joins for every forward reference.

[Diesel](https://diesel.rs/guides/relations.html) uses explicit association queries
and grouping into Rust values. Our generated relation handles and `FromRow`
results retain explicit asynchronous execution without property-access queries.

[Prisma](https://www.prisma.io/docs/orm/v7/prisma-schema/data-model/relations)
distinguishes relation scalar fields from relation fields. Our descriptor keeps
the logical name on the existing reference definition, giving Rust and V8 the
same edge authority without another graph declaration.

## Scope

Forward loading targets unique text or integer keys. It returns protected target
records or an absent value, preserving native integer and binary representations.
Reverse collections, many-to-many loading, nested includes, related projections
and graph writes are separate increments. The uncommitted feature roadmap tracks
those remaining capabilities.
