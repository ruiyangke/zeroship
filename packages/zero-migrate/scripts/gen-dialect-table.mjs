// Generate the single-source dialect-support table.
//
// Reads the hand-authored sidecar
// `crates/zero-migrate/dialect-support.toml` — one row per (op-kind,
// variant) with a per-dialect disposition — and emits BOTH downstream artifacts
// from that one source:
//   (a) crates/zero-migrate/src/model/dialect_table.rs - a const lookup the
//       engine reads through `model::op_support`'s `dialect_table::lookup`.
//   (b) packages/zero-migrate/src/generated/dialect-table.ts - the TS mirror,
//       which nothing outside its own file reads today; it exists so the SDK
//       can be pinned against the same sidecar.
//
// This mirrors the `gen-ir-types.mjs` flow: committed generated files + a
// regenerate-and-diff CI gate. Regenerate with:
//
//   pnpm --filter zero-migrate gen:dialect-table
//
// then commit the regenerated dialect_table.rs + dialect-table.ts.
//
// The faithfulness of the sidecar itself (that it mirrors the engine's live
// `Support::decision()`) is proven by the Rust test
// `crates/zero-migrate/tests/dialect_table_faithfulness.rs`; this script only
// transcribes the sidecar into the two typed artifacts.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const sidecarPath = resolve(here, "../../../crates/zero-migrate/dialect-support.toml");

// Output paths default to the committed artifacts; the drift test overrides them
// via env vars to regenerate into temp files and byte-compare (the "regenerate +
// diff" freshness gate, matching gen-ir-types' GEN_IR_OUT).
const rustOut = process.env.GEN_DIALECT_RUST_OUT
  ? resolve(process.env.GEN_DIALECT_RUST_OUT)
  : resolve(here, "../../../crates/zero-migrate/src/model/dialect_table.rs");
const tsOut = process.env.GEN_DIALECT_TS_OUT
  ? resolve(process.env.GEN_DIALECT_TS_OUT)
  : resolve(here, "../src/generated/dialect-table.ts");

const DISPOSITIONS = ["portable", "transparentDegradable", "vendor", "unsupported"];
const DISPOSITION_RUST = {
  portable: "Portable",
  transparentDegradable: "TransparentDegradable",
  vendor: "Vendor",
  unsupported: "Unsupported",
};

// ── A strict, purpose-built reader for the flat `[[row]]` sidecar subset. ──
// (No TOML dependency is installable offline; the sidecar is intentionally
// restricted to blank lines, `#` comments, `[[row]]` table-array headers, and
// `key = "string"` assignments, so this reader is total and throws loudly on any
// construct outside that subset — it can never silently misparse.)
function parseSidecar(text) {
  const rows = [];
  let cur = null;
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i];
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === "[[row]]") {
      cur = {};
      rows.push(cur);
      continue;
    }
    const m = line.match(/^([a-zA-Z][a-zA-Z0-9_]*)\s*=\s*"([^"\\]*)"\s*(#.*)?$/);
    if (!m) {
      throw new Error(`dialect-support.toml:${i + 1}: unsupported line: ${JSON.stringify(raw)}`);
    }
    if (!cur) {
      throw new Error(`dialect-support.toml:${i + 1}: key before any [[row]] header`);
    }
    if (Object.prototype.hasOwnProperty.call(cur, m[1])) {
      throw new Error(`dialect-support.toml:${i + 1}: duplicate key "${m[1]}" in row`);
    }
    cur[m[1]] = m[2];
  }
  return rows;
}

function validateRows(rows) {
  if (rows.length === 0) throw new Error("dialect-support.toml: no rows parsed");
  const seen = new Set();
  for (const row of rows) {
    for (const key of ["kind", "variant", "pg", "sqlite", "mysql"]) {
      if (typeof row[key] !== "string") {
        throw new Error(`dialect-support.toml: row missing "${key}": ${JSON.stringify(row)}`);
      }
    }
    const extra = Object.keys(row).filter(
      (k) => !["kind", "variant", "pg", "sqlite", "mysql"].includes(k),
    );
    if (extra.length) {
      throw new Error(`dialect-support.toml: row ${row.kind}/${row.variant} has unknown keys ${extra.join(",")}`);
    }
    for (const dialect of ["pg", "sqlite", "mysql"]) {
      if (!DISPOSITIONS.includes(row[dialect])) {
        throw new Error(
          `dialect-support.toml: row ${row.kind}/${row.variant} has invalid ${dialect} disposition "${row[dialect]}"`,
        );
      }
    }
    const id = `${row.kind}\0${row.variant}`;
    if (seen.has(id)) {
      throw new Error(`dialect-support.toml: duplicate (kind, variant) = (${row.kind}, ${row.variant})`);
    }
    seen.add(id);
  }
  // Deterministic order: by (kind, variant) so the generated artifacts are stable
  // regardless of sidecar row order.
  //
  // Compared by CODE UNIT, not `localeCompare`. The comparator has to be a
  // property of the data alone, because this ordering is baked into committed
  // artifacts that a drift gate then re-derives: a comparator that consults the
  // runtime locale or ICU build would let two contributors generate two orderings
  // from one sidecar and each see the other as drift. `localeCompare` also orders
  // case differently from code units ("a" before "B", rather than after), which is
  // exactly the axis camelCase keys vary on.
  //
  // Measured on the current 92 rows: locale and code-unit ordering agree, and so
  // do the `en` and `sv` collations. So this is closing a guarantee rather than
  // correcting today's output - the generated files do not change.
  rows.sort((a, b) => (a.kind === b.kind ? compareCodeUnits(a.variant, b.variant) : compareCodeUnits(a.kind, b.kind)));
}

/** Order two strings by UTF-16 code unit, independent of locale and ICU build. */
function compareCodeUnits(a, b) {
  if (a < b) return -1;
  if (a > b) return 1;
  return 0;
}

function esc(s) {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function emitRust(rows) {
  const banner = `//! GENERATED FILE — do not edit by hand.
//! Source: crates/zero-migrate/dialect-support.toml (the single-source
//! dialect-support sidecar). Regenerate with:
//!   pnpm --filter zero-migrate gen:dialect-table
//!
//! One [\`DispositionRow\`] per (op-kind, variant) recording the token's
//! disposition on each dialect. Faithfulness to the engine's live
//! \`Support::decision()\` is proven by
//! \`tests/dialect_table_faithfulness.rs\`. Engine code DOES read this table:
//! \`crate::model::op_support\` calls \`dialect_table::lookup(kind, variant)\`.
//! The TypeScript mirror has no reader outside its own file; state the two
//! sides separately, because they have already drifted apart once.
`;

  const body = `
use crate::model::support::Dialect;

/// The disposition of one (op-kind, variant) token on one dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Core construct that renders/validates on this dialect.
    Portable,
    /// Native where supported, absence-tolerable elsewhere. Reserved; no current
    /// row uses it.
    TransparentDegradable,
    /// Vendor-tier construct admitted on this dialect.
    Vendor,
    /// Refused on this dialect.
    Unsupported,
}

/// One row of the generated dialect table: an (op-kind, variant) token and its
/// per-dialect disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispositionRow {
    /// The op-kind wire token (e.g. \`"createTable"\`).
    pub kind: &'static str,
    /// The variant token distinguishing payload-dependent branches; \`"base"\`
    /// for payload-independent ops.
    pub variant: &'static str,
    /// Disposition on PostgreSQL.
    pub postgres: Disposition,
    /// Disposition on SQLite.
    pub sqlite: Disposition,
    /// Disposition on MySQL.
    pub mysql: Disposition,
}

impl DispositionRow {
    /// The disposition of this row on the given dialect.
    #[must_use]
    pub const fn disposition(&self, dialect: Dialect) -> Disposition {
        match dialect {
            Dialect::Postgres => self.postgres,
            Dialect::Sqlite => self.sqlite,
            Dialect::Mysql => self.mysql,
        }
    }
}

/// The generated dialect table, sorted by (kind, variant).
///
/// \`#[rustfmt::skip]\` keeps each row on one line: this file is generator-owned
/// (the drift test byte-compares it against \`gen:dialect-table\`), and letting
/// \`cargo fmt\` reflow the rows would put the committed form permanently at odds
/// with the generator's output.
#[rustfmt::skip]
pub const DIALECT_TABLE: &[DispositionRow] = &[
${rows
  .map(
    (r) =>
      `    DispositionRow { kind: "${esc(r.kind)}", variant: "${esc(r.variant)}", postgres: Disposition::${DISPOSITION_RUST[r.pg]}, sqlite: Disposition::${DISPOSITION_RUST[r.sqlite]}, mysql: Disposition::${DISPOSITION_RUST[r.mysql]} },`,
  )
  .join("\n")}
];

/// Look up the row for an (op-kind, variant) token, if present.
#[must_use]
pub fn lookup(kind: &str, variant: &str) -> Option<&'static DispositionRow> {
    DIALECT_TABLE
        .iter()
        .find(|row| row.kind == kind && row.variant == variant)
}
`;
  return banner + body;
}

function emitTs(rows) {
  const banner = `/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zero-migrate/dialect-support.toml (the single-source
// dialect-support sidecar). Regenerate with:
//   pnpm --filter zero-migrate gen:dialect-table
//
// One row per (op-kind, variant) recording the token's disposition on each
// dialect — the TS mirror of crates/zero-migrate/src/model/dialect_table.rs.
// Faithfulness to the engine's live Support::decision() is proven Rust-side by
// tests/dialect_table_faithfulness.rs; the TS drift test pins this file (and the
// Rust one) against the sidecar. NOTHING outside this file reads the TS mirror;
// the Rust table, by contrast, is read by the engine's op_support lookup.
`;

  const body = `
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
${rows
  .map(
    (r) =>
      `  { kind: "${esc(r.kind)}", variant: "${esc(r.variant)}", postgres: "${r.pg}", sqlite: "${r.sqlite}", mysql: "${r.mysql}" },`,
  )
  .join("\n")}
] as const;

export function lookupDisposition(kind: string, variant: string): DispositionRow | undefined {
  return DIALECT_TABLE.find((row) => row.kind === kind && row.variant === variant);
}
`;
  return banner + body;
}

const text = await readFile(sidecarPath, "utf8");
const rows = parseSidecar(text);
validateRows(rows);

await mkdir(dirname(rustOut), { recursive: true });
await mkdir(dirname(tsOut), { recursive: true });
await writeFile(rustOut, emitRust(rows), "utf8");
await writeFile(tsOut, emitTs(rows), "utf8");
console.log(`wrote ${rustOut}`);
console.log(`wrote ${tsOut} (${rows.length} rows)`);
