/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/dialect-support.toml (the single-source
// dialect-support sidecar). Regenerate with:
//   pnpm --filter @zeroship/migrate gen:dialect-table
//
// One row per (op-kind, variant) recording the token's disposition on each
// dialect, KEYED BY DIALECT ID — the TS mirror of
// crates/zeroship-migrate/tests/dialect_matrix/dialect_table.rs.
//
// There is deliberately NO `Dialect` union here. A closed union of the shipping
// dialect names is the same "core enumerates the vendors" shape as a struct field
// per vendor: it would have to be widened by hand for a fourth backend, and every
// consumer narrowing on it would silently not cover the new one. The key type is
// `string` (a dialect id) and the census lives in the DATA.
//
// The TS drift test pins this file (and the Rust one) against the sidecar, and
// carries the census floor that a keyed-by-data scan needs. NOTHING outside this
// file reads the TS mirror. Production Rust support decisions likewise come from
// the selected registered backend, not from the generated Rust artifact.

export type Disposition = "portable" | "transparentDegradable" | "vendor" | "unsupported";

export interface DispositionRow {
  readonly kind: string;
  readonly variant: string;
  /** Disposition per dialect id (e.g. `"postgres"`), the id being the same
   *  canonical spelling `DialectId` uses Rust-side. */
  readonly dispositions: Readonly<Record<string, Disposition>>;
}

export const DIALECT_TABLE: readonly DispositionRow[] = [
  { kind: "addColumn", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "addColumn", variant: "identity", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "addColumn", variant: "nextvalDefault", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "addConstraint", variant: "check", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "addConstraint", variant: "exclusion", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "addConstraint", variant: "fkComposite", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "addConstraint", variant: "fkNoLocalColumn", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "addConstraint", variant: "fkNonId", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "addConstraint", variant: "fkNotValid", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "addConstraint", variant: "fkSimple", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "addConstraint", variant: "unique", dispositions: { mysql: "portable", postgres: "portable", sqlite: "unsupported" } },
  { kind: "alterPrimaryKey", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "alterRole", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "alterSequence", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "attachPartition", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "backfill", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "comment", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createDomain", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "createDomain", variant: "nextvalDefault", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createEnum", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "createExtension", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createFunction", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createIndex", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "createIndex", variant: "exprElement", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "portable" } },
  { kind: "createIndex", variant: "partialWhere", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "portable" } },
  { kind: "createIndex", variant: "pgOnlyMethodOrFeature", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createPartition", variant: "base", dispositions: { mysql: "transparentDegradable", postgres: "portable", sqlite: "transparentDegradable" } },
  { kind: "createPolicy", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createRole", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createRole", variant: "superuserIfNotExists", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "createSchema", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createSequence", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTable", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "createTable", variant: "identityAlways", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTable", variant: "nextvalDefault", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTable", variant: "nonportableByDefaultIdentity", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTable", variant: "partitioned", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTable", variant: "partitionedCollapse", dispositions: { mysql: "transparentDegradable", postgres: "portable", sqlite: "transparentDegradable" } },
  { kind: "createTable", variant: "pgOnlyIndexFeature", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createTrigger", variant: "bodyInsteadOf", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "portable" } },
  { kind: "createTrigger", variant: "bodyMultipleEvents", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "createTrigger", variant: "bodyRaiseIgnore", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "portable" } },
  { kind: "createTrigger", variant: "bodySimple", dispositions: { mysql: "portable", postgres: "unsupported", sqlite: "portable" } },
  { kind: "createTrigger", variant: "bodyStatementLevel", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "createTrigger", variant: "bodyTruncateEvent", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "createTrigger", variant: "bodyWhen", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "portable" } },
  { kind: "createTrigger", variant: "executeFunction", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "createView", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "createView", variant: "materialized", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "createView", variant: "materializedReplace", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "delete", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "detachPartition", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "dialectal", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropColumn", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropColumnDefault", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "unsupported" } },
  { kind: "dropColumnNotNull", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "dropConstraint", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropDomain", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropEnum", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropExtension", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropFunction", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropIndex", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropOwnedBy", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropPartition", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropPolicy", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropRole", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropSchema", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "dropSequence", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "dropTable", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropTrigger", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropView", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "dropView", variant: "materialized", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "grant", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "insert", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "insert", variant: "onConflictDoNothing", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "portable" } },
  { kind: "insert", variant: "onConflictDoUpdate", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "raw", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "renameColumn", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "portable" } },
  { kind: "renameColumn", variant: "existenceGuard", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "renameTable", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "revoke", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "setColumnDefault", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "unsupported" } },
  { kind: "setColumnDefault", variant: "containerOrJson", dispositions: { mysql: "portable", postgres: "portable", sqlite: "unsupported" } },
  { kind: "setColumnDefault", variant: "nextval", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "setColumnNotNull", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
  { kind: "setColumnType", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "unsupported" } },
  { kind: "setColumnType", variant: "using", dispositions: { mysql: "unsupported", postgres: "unsupported", sqlite: "unsupported" } },
  { kind: "setRls", variant: "base", dispositions: { mysql: "unsupported", postgres: "vendor", sqlite: "unsupported" } },
  { kind: "setTableOptions", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "synchronizeIdentity", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "update", variant: "base", dispositions: { mysql: "portable", postgres: "portable", sqlite: "portable" } },
  { kind: "validateConstraint", variant: "base", dispositions: { mysql: "unsupported", postgres: "portable", sqlite: "unsupported" } },
] as const;

export function lookupDisposition(kind: string, variant: string): DispositionRow | undefined {
  return DIALECT_TABLE.find((row) => row.kind === kind && row.variant === variant);
}
