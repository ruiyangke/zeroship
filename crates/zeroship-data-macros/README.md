# Rust ORM macros

`schema!` declares native Rust collection and column metadata. `FromRow`,
`Insertable`, and `Changeset` generate native
mapping implementations checked against that metadata. The macros are re-exported
by `zeroship_data_orm::orm`; applications use that entry point.

```rust
use zeroship_data_orm::orm::schema;

schema! {
    pub models {
        users {
            #[orm(primary_key, assign(on = insert, by = typed_id))]
            id: Text,
            name: Text,
            email: Nullable<Text>,
        }
    }
}
```

`models::schema()` returns the native metadata used to bind a database. Fields
are required unless declared `Nullable<T>`. Assignment generators, defaults,
protection, and references are explicit column attributes. Named edges use
`references(users::id), relation(author)`. Object and union metadata use nested
`shape(...)` and `variants({ ... }, ...)` field declarations.

`src/schema.rs` emits metadata and typed capabilities from the declarations parsed
in `src/schema/input.rs`. `src/derive.rs` owns mapping derives, field aliases,
defaults, generic bounds, and attribute diagnostics. The crate has no SQL,
driver, or runtime dependency.

Runtime semantics live in `zeroship-data-orm`, so handwritten mappings and
macro-generated mappings use the same codecs and execution path. Usage is in
`crates/zeroship-data-orm/README.md`; compilation contracts and database round
trips are tested by that crate.
