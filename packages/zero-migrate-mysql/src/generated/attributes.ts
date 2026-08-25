// GENERATED FILE — DO NOT EDIT.
//
// Source: the `attribute-vocabulary.json` exported by this backend's Rust crate from
// its own `static DEFS`. Regenerate with:
//
//     UPDATE_VOCABULARY=1 cargo test -p zero-migrate-mysql --test attribute_vocabulary_export
//     node packages/zero-migrate-mysql/scripts/gen-attributes.mjs
//
// A drift test asserts this file matches the artifact, so an edit here is reverted by
// the next regeneration rather than silently kept.
//
// This module augments the neutral `zero-migrate` package's `VendorAttributeNamespaces`
// interface. Importing this package is what makes `mysql: { … }` typecheck on the
// authoring surface; without it the key is a type error. The namespace key is the
// backend's DIALECT ID — there is no hand-picked alias anywhere in the chain.

declare module "zero-migrate" {
  /** Table-level options this backend accepts. */
  interface MysqlTableAttributes {
    /**
     * The storage engine. Anything other than InnoDB gives up transactional DDL-adjacent
     * guarantees this tool otherwise relies on.
     */
    engine?: "InnoDB" | "MyISAM" | "MEMORY" | "CSV" | "ARCHIVE";

    /**
     * How rows are physically stored. DYNAMIC and COMPRESSED allow longer index keys over
     * variable-length columns than REDUNDANT or COMPACT.
     */
    row_format?: "DEFAULT" | "DYNAMIC" | "FIXED" | "COMPRESSED" | "REDUNDANT" | "COMPACT";

    /**
     * The next value the table's AUTO_INCREMENT column will hand out.
     *
     * Accepted range: 0..=9223372036854775807 (enforced when the migration is planned, not
     * by this type).
     */
    auto_increment?: number;

    /**
     * The table's DEFAULT CHARACTER SET, inherited by character columns that do not name
     * their own.
     */
    charset?: string;

    /**
     * The table's default collation. Decides comparison and sort order, and therefore
     * whether a unique key treats two spellings as one value.
     */
    collate?: string;
  }

  interface VendorAttributeNamespaces {
    /**
     * Table options specific to the `mysql` backend.
     *
     * Present because this package is installed. Every field is optional, and a table
     * that also carries other backends' options stays portable to all of them.
     */
    mysql?: MysqlTableAttributes;
  }
}

export {};
