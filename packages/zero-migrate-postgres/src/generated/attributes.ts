// GENERATED FILE — DO NOT EDIT.
//
// Source: the `attribute-vocabulary.json` exported by this backend's Rust crate from
// its own `static DEFS`. Regenerate with:
//
//     UPDATE_VOCABULARY=1 cargo test -p zeroship-migrate-postgres --test attribute_vocabulary_export
//     node packages/zero-migrate-postgres/scripts/gen-attributes.mjs
//
// A drift test asserts this file matches the artifact, so an edit here is reverted by
// the next regeneration rather than silently kept.
//
// This module augments the neutral `zero-migrate` package's `VendorAttributeNamespaces`
// interface. Importing this package is what makes `postgres: { … }` typecheck on the
// authoring surface; without it the key is a type error. The namespace key is the
// backend's DIALECT ID — there is no hand-picked alias anywhere in the chain.

declare module "zero-migrate" {
  /** CreateTable-level options this backend accepts. */
  interface PostgresCreateTableAttributes {
    /**
     * Percentage of each page left free for later updates, so a row can be updated in
     * place. 100 packs pages fully and suits an insert-only table.
     *
     * Accepted range: 10..=100 (enforced when the migration is planned, not by this type).
     */
    fillfactor?: number;

    /**
     * The tablespace the table is created in. Must already exist on the server.
     */
    tablespace?: string;

    /**
     * Whether autovacuum runs on this table. Disabling it makes vacuuming the operator's
     * problem and is rarely right.
     */
    autovacuum_enabled?: boolean;

    /**
     * Row length above which PostgreSQL tries to move columns out of line into TOAST
     * storage. The upper bound is the server's block size minus its header (8160 on a
     * default 8kB build); a larger block size accepts more than this declaration allows.
     *
     * Accepted range: 128..=8160 (enforced when the migration is planned, not by this
     * type).
     */
    toast_tuple_target?: number;

    /**
     * How many workers a parallel scan of this table should ask for. 0 disables parallel
     * scans of it. The server clamps the effective count against max_parallel_workers, so
     * a high value here is a request, not a guarantee.
     *
     * Accepted range: 0..=2147483647 (enforced when the migration is planned, not by this
     * type).
     */
    parallel_workers?: number;
  }

  interface VendorAttributeNamespaces {
    /**
     * CreateTable options specific to the `postgres` backend.
     *
     * Present because this package is installed. Every field is optional, and an object
     * that also carries other backends' options stays portable to all of them.
     */
    postgres?: PostgresCreateTableAttributes;
  }

  /** CreateIndex-level options this backend accepts. */
  interface PostgresCreateIndexAttributes {
    /**
     * Percentage of each index page left free when the index is built, so a later insert
     * can go on the right page instead of splitting it.
     *
     * Accepted range: 10..=100 (enforced when the migration is planned, not by this type).
     */
    fillfactor?: number;

    /**
     * BRIN only: how many table blocks each index entry summarises. A smaller range makes
     * a larger but more selective index.
     *
     * Accepted range: 1..=131072 (enforced when the migration is planned, not by this
     * type).
     */
    pages_per_range?: number;
  }

  interface VendorIndexAttributeNamespaces {
    /**
     * CreateIndex options specific to the `postgres` backend.
     *
     * Present because this package is installed. Every field is optional, and an object
     * that also carries other backends' options stays portable to all of them.
     */
    postgres?: PostgresCreateIndexAttributes;
  }
}

export {};
