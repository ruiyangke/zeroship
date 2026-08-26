// Generate the single-source dialect-support table.
//
// Reads the hand-authored sidecar
// `crates/zeroship-migrate/dialect-support.toml` — one row per (op-kind,
// variant) with a per-dialect disposition — and emits BOTH review/parity
// artifacts from that one source. Production support decisions live in each
// registered backend's required ValidationPolicy instead.
//
// NOTHING HERE NAMES A DIALECT. `kind` and `variant` are the row's two structural
// keys; every other key is a DIALECT ID, carried through to both artifacts as a
// map key rather than a field named after a vendor. That is the whole point: a
// fourth backend adds a column to the sidecar and this script does not change.
//
// The artifacts:
//   (a) crates/zeroship-migrate/tests/dialect_matrix/dialect_table.rs - the Rust
//       integration-test review artifact; its parity gate compares every generated
//       cell with the registered backend policies.
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
// This script only transcribes the sidecar into the two typed artifacts. What
// proves the sidecar itself is split across three tests, and naming one of them
// for all three is how a gap hides:
//   * `crates/zeroship-migrate/tests/dialect_matrix/dialect_table_faithfulness.rs` —
//     corpus ⟷ table bijection and sidecar ⟷ table transcription.
//   * `generated_cells_match_registered_backend_policies` in the generated Rust
//     artifact — all generated cells ⟷ the registered backends' required policy
//     answers. This is no longer tautological: production never reads the table.
//   * `op_support_matrix.rs` — the behavioural gate (a decision matches what
//     validate/lower actually do).
//   * `dialect_conformance_live.rs` — the same, against real servers.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
// The sidecar path is overridable for the SAME reason the outputs are: the
// open-dialect gate in `tests/dialect-table-drift.test.ts` drives a synthetic
// sidecar naming a dialect this script has never heard of through this script
// UNEDITED. Without the override that gate could only be written by editing the
// generator it is supposed to be testing.
const sidecarPath = process.env.GEN_DIALECT_SIDECAR
  ? resolve(process.env.GEN_DIALECT_SIDECAR)
  : resolve(here, "../../../crates/zeroship-migrate/dialect-support.toml");

// Output paths default to the committed artifacts; the drift test overrides them
// via env vars to regenerate into temp files and byte-compare (the "regenerate +
// diff" freshness gate, matching gen-ir-types' GEN_IR_OUT).
const rustOut = process.env.GEN_DIALECT_RUST_OUT
  ? resolve(process.env.GEN_DIALECT_RUST_OUT)
  : resolve(here, "../../../crates/zeroship-migrate/tests/dialect_matrix/dialect_table.rs");
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

/** The two STRUCTURAL keys of a row. EVERY other key is a dialect id. */
const STRUCTURAL_KEYS = ["kind", "variant"];

/** The `DialectId` rule from `zero_migrate_ir::dialect`: lowercase `[a-z][a-z0-9_]*`. */
const DIALECT_ID = /^[a-z][a-z0-9_]*$/;

/** The dialect ids a row declares, sorted by code unit. */
function dialectIdsOf(row) {
  return Object.keys(row)
    .filter((k) => !STRUCTURAL_KEYS.includes(k))
    .sort(compareCodeUnits);
}

function validateRows(rows) {
  if (rows.length === 0) throw new Error("dialect-support.toml: no rows parsed");
  const seen = new Set();
  // The dialect census is DISCOVERED from the first row, then every other row is
  // required to match it. Nothing here names a vendor: a fourth backend adds its
  // column to every row and this function learns the id from the data.
  //
  // Requiring all rows to agree is the CENSUS FLOOR. Per-row discovery alone would
  // fail open — a row that lost a cell would simply declare fewer dialects and
  // generate cleanly, and the artifact would silently make no claim where it used
  // to make one. The old three hard-coded keys prevented that by construction; this
  // is what replaces them.
  let census = null;
  for (const row of rows) {
    for (const key of STRUCTURAL_KEYS) {
      if (typeof row[key] !== "string") {
        throw new Error(`dialect-support.toml: row missing "${key}": ${JSON.stringify(row)}`);
      }
    }
    const dialects = dialectIdsOf(row);
    if (dialects.length === 0) {
      throw new Error(`dialect-support.toml: row ${row.kind}/${row.variant} declares no dialects`);
    }
    for (const dialect of dialects) {
      if (!DIALECT_ID.test(dialect)) {
        throw new Error(
          `dialect-support.toml: row ${row.kind}/${row.variant} key "${dialect}" is not a well-formed DialectId ([a-z][a-z0-9_]*)`,
        );
      }
      if (!DISPOSITIONS.includes(row[dialect])) {
        throw new Error(
          `dialect-support.toml: row ${row.kind}/${row.variant} has invalid ${dialect} disposition "${row[dialect]}"`,
        );
      }
    }
    if (census === null) {
      census = dialects;
    } else if (census.join("\0") !== dialects.join("\0")) {
      throw new Error(
        `dialect-support.toml: row ${row.kind}/${row.variant} declares dialects [${dialects.join(",")}] but the table's census is [${census.join(",")}] — every row must declare every dialect`,
      );
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
//! Source: crates/zeroship-migrate/dialect-support.toml (the single-source
//! dialect-support sidecar). Regenerate with:
//!   pnpm --filter zero-migrate gen:dialect-table
//!
//! One [\`DispositionRow\`] per (op-kind, variant) recording the token's
//! disposition on each dialect, KEYED BY [\`DialectId\`] rather than by one struct
//! field per vendor. That keying is the point: a fourth backend adds a column to
//! the sidecar and nothing here, in the generator, or in core changes shape.
//!
//! Which test proves what: \`tests/dialect_table_faithfulness.rs\` proves the
//! corpus ⟷ table bijection and the sidecar ⟷ table transcription. The integration
//! test below compares all generated cells with the registered backends' required
//! policies. \`op_support_matrix.rs\` is the behavioural gate;
//! \`dialect_conformance_live.rs\` is the live one.
//!
//! Production engine code DOES NOT read this table: it resolves the selected
//! registered vendor and calls that vendor's required \`ValidationPolicy\`.
//! This Rust table and its TypeScript mirror are generator/test artifacts only.
`;

  const body = `
pub use zero_migrate_backend::validation::Disposition;
use zero_migrate_ir::dialect::DialectId;

/// One row of the generated dialect table: an (op-kind, variant) token and its
/// per-dialect disposition.
///
/// The dispositions are an ASSOCIATION LIST keyed by [\`DialectId\`], not one field
/// per vendor. The previous shape put every dialect this engine ships in the type
/// itself, so a fourth backend could not declare its dispositions without editing
/// a struct in a crate it does not own — the same closed-set problem
/// [\`DialectId\`] exists to remove, in a shape that is not an enum and so was not
/// removed by deleting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispositionRow {
    /// The op-kind wire token (e.g. \`"createTable"\`).
    pub kind: &'static str,
    /// The variant token distinguishing payload-dependent branches; \`"base"\`
    /// for payload-independent ops.
    pub variant: &'static str,
    /// This token's disposition per dialect, sorted by [\`DialectId\`] and
    /// deduplicated — the same sorted-slice discipline
    /// [\`zero_migrate_ir::dialect::DialectSet\`] uses — so lookup is a binary
    /// search and the emitted order is stable.
    pub dispositions: &'static [(DialectId, Disposition)],
}

impl DispositionRow {
    /// The disposition this row declares for \`id\`, or \`None\` if it declares none.
    ///
    /// \`None\` means THE TABLE MAKES NO CLAIM, which is not the same as
    /// \`Unsupported\` (an explicit refusal). Callers that need a verdict must
    /// decide which they mean; [\`Self::disposition\`] panics rather than pick one
    /// silently.
    #[must_use]
    pub fn disposition_for(&self, id: &DialectId) -> Option<Disposition> {
        self.dispositions
            .binary_search_by(|(dialect, _)| dialect.cmp(id))
            .ok()
            .map(|i| self.dispositions[i].1)
    }

    /// The disposition of this row on the given dialect.
    ///
    /// Panics if the row declares no cell for it. A missing cell is a generation
    /// defect, and both of the other answers hide it — treating it as supported
    /// fails open, and treating it as \`Unsupported\` invents a refusal the
    /// sidecar never authored.
    #[must_use]
    pub fn disposition(&self, dialect: &DialectId) -> Disposition {
        self.disposition_for(dialect).unwrap_or_else(|| {
            panic!(
                "dialect table row {}/{} declares no disposition for {dialect}",
                self.kind, self.variant
            )
        })
    }

    /// The dialects this row declares a disposition for, in ascending id order.
    pub fn dialects(&self) -> impl Iterator<Item = DialectId> + '_ {
        self.dispositions.iter().map(|(id, _)| id.clone())
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
      `    DispositionRow { kind: "${esc(r.kind)}", variant: "${esc(r.variant)}", dispositions: &[${dialectIdsOf(
        r,
      )
        .map((d) => `(DialectId::new("${esc(d)}"), Disposition::${DISPOSITION_RUST[r[d]]})`)
        .join(", ")}] },`,
  )
  .join("\n")}
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated three-vendor artifact remains a byte-pinned review surface,
    /// but production asks each registered backend directly. Pin every one of the
    /// 92 × 3 historical decisions while ownership moves across that boundary.
    #[test]
    fn generated_cells_match_registered_backend_policies() {
        assert_eq!(
            DIALECT_TABLE.len(),
            92,
            "the reviewed operation-shape census moved"
        );
        assert_eq!(
            crate::SHIPPING_VENDORS.len(),
            3,
            "the reviewed shipping-backend census moved"
        );

        let mut checked = 0;
        for row in DIALECT_TABLE {
            assert_eq!(
                row.dispositions.len(),
                crate::SHIPPING_VENDORS.len(),
                "generated row {}/{} does not cover every registered backend",
                row.kind,
                row.variant,
            );
            for vendor in crate::SHIPPING_VENDORS {
                let dialect = &vendor.descriptor.id;
                let expected = row.disposition_for(dialect).unwrap_or_else(|| {
                    panic!(
                        "generated row {}/{} omits backend {dialect}",
                        row.kind, row.variant
                    )
                });
                assert_eq!(
                    vendor.validation.op_disposition(row.kind, row.variant),
                    expected,
                    "backend policy drifted from generated cell {}/{}/{}",
                    row.kind,
                    row.variant,
                    dialect,
                );
                checked += 1;
            }
        }

        assert_eq!(checked, 276, "the reviewed generated-cell census moved");
    }
}
`;
  return banner + body;
}

function emitTs(rows) {
  const banner = `/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/dialect-support.toml (the single-source
// dialect-support sidecar). Regenerate with:
//   pnpm --filter zero-migrate gen:dialect-table
//
// One row per (op-kind, variant) recording the token's disposition on each
// dialect, KEYED BY DIALECT ID — the TS mirror of
// crates/zeroship-migrate/tests/dialect_matrix/dialect_table.rs.
//
// There is deliberately NO \`Dialect\` union here. A closed union of the shipping
// dialect names is the same "core enumerates the vendors" shape as a struct field
// per vendor: it would have to be widened by hand for a fourth backend, and every
// consumer narrowing on it would silently not cover the new one. The key type is
// \`string\` (a dialect id) and the census lives in the DATA.
//
// The TS drift test pins this file (and the Rust one) against the sidecar, and
// carries the census floor that a keyed-by-data scan needs. NOTHING outside this
// file reads the TS mirror. Production Rust support decisions likewise come from
// the selected registered backend, not from the generated Rust artifact.
`;

  const body = `
export type Disposition = "portable" | "transparentDegradable" | "vendor" | "unsupported";

export interface DispositionRow {
  readonly kind: string;
  readonly variant: string;
  /** Disposition per dialect id (e.g. \`"postgres"\`), the id being the same
   *  canonical spelling \`DialectId\` uses Rust-side. */
  readonly dispositions: Readonly<Record<string, Disposition>>;
}

export const DIALECT_TABLE: readonly DispositionRow[] = [
${rows
  .map(
    (r) =>
      `  { kind: "${esc(r.kind)}", variant: "${esc(r.variant)}", dispositions: { ${dialectIdsOf(
        r,
      )
        .map((d) => `${d}: "${r[d]}"`)
        .join(", ")} } },`,
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
