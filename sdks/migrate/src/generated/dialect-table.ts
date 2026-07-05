/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/dialect-support.toml (the single-source
// dialect-support sidecar). Regenerate with:
//   pnpm --filter @zeroship/migrate gen:dialect-table
//
// One row per (op-kind, variant) recording the token's disposition on each
// dialect — the TS mirror of crates/zeroship-migrate/src/model/dialect_table.rs.
// Faithfulness to the engine's live Support::decision() is proven Rust-side by
// tests/dialect_table_faithfulness.rs; the TS drift test pins this file (and the
// Rust one) against the sidecar. S0.1 is ADDITIVE — no consumer reads it yet.

export type Disposition = "portable" | "transparentDegradable" | "vendor" | "unsupported";
export type Dialect = "postgres" | "sqlite" | "mysql";

export interface DispositionRow {
  readonly kind: string;
  readonly variant: string;
  readonly postgres: Disposition;
  readonly sqlite: Disposition;
  readonly mysql: Disposition;
}

export const DIALECT_TABLE: readonly DispositionRow[] = [
  { kind: "addColumn", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "addColumn", variant: "identity", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addColumn", variant: "nextvalDefault", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "check", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "exclusion", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "fkComposite", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "fkNoLocalColumn", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "fkNonId", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "fkNotValid", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "addConstraint", variant: "fkSimple", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "addConstraint", variant: "unique", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "alterRole", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "alterSequence", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "backfill", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "comment", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createDomain", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "createDomain", variant: "nextvalDefault", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createEnum", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "createExtension", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createFunction", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createIndex", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "createIndex", variant: "exprElement", postgres: "portable", sqlite: "portable", mysql: "unsupported" },
  { kind: "createIndex", variant: "partialWhere", postgres: "portable", sqlite: "portable", mysql: "unsupported" },
  { kind: "createIndex", variant: "pgOnlyMethodOrFeature", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createPartition", variant: "base", postgres: "portable", sqlite: "transparentDegradable", mysql: "transparentDegradable" },
  { kind: "createPolicy", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createRole", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createRole", variant: "superuserIfNotExists", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createSchema", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createSequence", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTable", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "createTable", variant: "identityAlways", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTable", variant: "nextvalDefault", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTable", variant: "nonportableByDefaultIdentity", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTable", variant: "partitioned", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTable", variant: "partitionedCollapse", postgres: "portable", sqlite: "transparentDegradable", mysql: "transparentDegradable" },
  { kind: "createTable", variant: "pgOnlyIndexFeature", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodyInsteadOf", postgres: "unsupported", sqlite: "portable", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodyMultipleEvents", postgres: "unsupported", sqlite: "portable", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodyRaiseIgnore", postgres: "unsupported", sqlite: "portable", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodySimple", postgres: "unsupported", sqlite: "portable", mysql: "portable" },
  { kind: "createTrigger", variant: "bodyStatementLevel", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodyTruncateEvent", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createTrigger", variant: "bodyWhen", postgres: "unsupported", sqlite: "portable", mysql: "unsupported" },
  { kind: "createTrigger", variant: "executeFunction", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createView", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "createView", variant: "materialized", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "createView", variant: "materializedReplace", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "delete", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "detachPartition", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "disableRls", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropColumn", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropColumnDefault", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropColumnNotNull", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropConstraint", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropDomain", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropEnum", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropExtension", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropFunction", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropIndex", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropOwnedBy", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropPartition", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropPolicy", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropRole", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropSchema", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropSequence", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "dropTable", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropTrigger", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropView", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "dropView", variant: "materialized", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "enableRls", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "forceRls", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "grant", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "insert", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "insert", variant: "onConflict", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "noForceRls", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "pgRaw", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "renameColumn", variant: "base", postgres: "portable", sqlite: "portable", mysql: "unsupported" },
  { kind: "renameColumn", variant: "existenceGuard", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "renameTable", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "revoke", variant: "base", postgres: "vendor", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "setColumnDefault", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "setColumnDefault", variant: "containerOrJson", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "setColumnDefault", variant: "nextval", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "setColumnNotNull", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "setColumnType", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "setColumnType", variant: "using", postgres: "unsupported", sqlite: "unsupported", mysql: "unsupported" },
  { kind: "setTableOptions", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "update", variant: "base", postgres: "portable", sqlite: "portable", mysql: "portable" },
  { kind: "validateConstraint", variant: "base", postgres: "portable", sqlite: "unsupported", mysql: "unsupported" },
] as const;

export function lookupDisposition(kind: string, variant: string): DispositionRow | undefined {
  return DIALECT_TABLE.find((row) => row.kind === kind && row.variant === variant);
}
