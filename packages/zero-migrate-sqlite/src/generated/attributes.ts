// GENERATED FILE — DO NOT EDIT.
//
// Source: the `attribute-vocabulary.json` exported by this backend's Rust crate from
// its own `static DEFS`. Regenerate with:
//
//     UPDATE_VOCABULARY=1 cargo test -p zero-migrate-sqlite --test attribute_vocabulary_export
//     node packages/zero-migrate-sqlite/scripts/gen-attributes.mjs
//
// A drift test asserts this file matches the artifact, so an edit here is reverted by
// the next regeneration rather than silently kept.
//
// This module augments the neutral `zero-migrate` package's `VendorAttributeNamespaces`
// interface. Importing this package is what makes `sqlite: { … }` typecheck on the
// authoring surface; without it the key is a type error. The namespace key is the
// backend's DIALECT ID — there is no hand-picked alias anywhere in the chain.

declare module "zero-migrate" {
  /** CreateTable-level options this backend accepts. */
  interface SqliteCreateTableAttributes {
    /**
     * SQLite's STRICT table clause: enforce each column's declared type on write rather
     * than applying type affinity. Unrelated to zero-migrate's own deploy-time
     * `strictness` option.
     */
    strict?: boolean;

    /**
     * Store the table as an index over its PRIMARY KEY with no separate rowid. Requires a
     * PRIMARY KEY, and changes what a rowid-dependent query sees.
     */
    without_rowid?: boolean;
  }

  interface VendorAttributeNamespaces {
    /**
     * CreateTable options specific to the `sqlite` backend.
     *
     * Present because this package is installed. Every field is optional, and an object
     * that also carries other backends' options stays portable to all of them.
     */
    sqlite?: SqliteCreateTableAttributes;
  }
}

export {};
