/**
 * `defineMigration` — factory for a typed Migration descriptor.
 *
 * Returns a frozen object that `migrations.run` / `.status` / `.cancel`
 * accept. Names are stable (`migration.name`); the same descriptor can
 * be re-passed across worker restarts and the native side picks up the
 * persisted cursor by `(collection, name)`.
 */

import type { Migration, MigrateContext, PlainObject } from "./types";

/** Input shape — mirrors `Migration` but lets callers omit defaults. */
export interface DefineMigrationInput<
  Row extends PlainObject = PlainObject,
  Update extends PlainObject = PlainObject,
> {
  name: string;
  collection: string;
  batchSize?: number;
  failureBudget?: number;
  migrateOne: (row: Row, ctx: MigrateContext) => Update | undefined | null | Promise<Update | undefined | null>;
}

const DEFAULT_BATCH_SIZE = 200;

/**
 * Build a Migration descriptor from a plain spec.
 *
 * @throws Error if `name` or `collection` is empty / non-string, or
 *         `batchSize` is not a positive integer.
 */
export function defineMigration<Row extends PlainObject, Update extends PlainObject>(
  input: DefineMigrationInput<Row, Update>,
): Migration<Row, Update> {
  if (typeof input.name !== "string" || input.name.length === 0) {
    throw new Error("defineMigration: `name` must be a non-empty string");
  }
  if (typeof input.collection !== "string" || input.collection.length === 0) {
    throw new Error("defineMigration: `collection` must be a non-empty string");
  }
  const batchSize = input.batchSize ?? DEFAULT_BATCH_SIZE;
  if (!Number.isInteger(batchSize) || batchSize <= 0 || batchSize > 10_000) {
    throw new Error("defineMigration: `batchSize` must be an integer in (0, 10000]");
  }
  if (typeof input.migrateOne !== "function") {
    throw new Error("defineMigration: `migrateOne` must be a function");
  }

  return Object.freeze({
    name: input.name,
    collection: input.collection,
    batchSize,
    migrateOne: input.migrateOne,
    failureBudget: input.failureBudget ?? 0,
  });
}
