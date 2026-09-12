# Rust ORM macros

`schema!` reads a migration-generated runtime descriptor and emits Rust collection
and column metadata. `FromRow`, `Insertable`, and `Changeset` generate native
mapping implementations checked against that metadata. The macros are re-exported
by `zeroship_data_orm::orm`; applications use that entry point.

`src/schema.rs` owns artifact loading and metadata generation. Paths follow
`include_str!` semantics and the generated code tracks the artifact as a compiler
input. `src/derive.rs` owns mapping derives, field aliases, defaults, generic
bounds, and attribute diagnostics. The crate has no SQL, driver, or runtime
dependency. Shared descriptor fixtures check that its collection identity
validation agrees with ORM installation.

Runtime semantics live in `zeroship-data-orm`, so handwritten mappings and
macro-generated mappings use the same codecs and execution path. Usage is in
`crates/zeroship-data-orm/README.md`; compilation contracts and database round
trips are tested by that crate.
