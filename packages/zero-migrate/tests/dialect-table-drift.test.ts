// Drift guard for the generated dialect table.
//
// The single source is `crates/zero-migrate/dialect-support.toml`; the
// generator (`scripts/gen-dialect-table.mjs`) emits BOTH the committed TS mirror
// (`src/generated/dialect-table.ts`) and the committed Rust table
// (`crates/zero-migrate/tests/dialect_matrix/dialect_table.rs`). This test is the
// "regenerate + diff" freshness gate (the same shape as ir-types-drift's enums
// gate): re-run the generator into temp files and assert byte-equality with both
// committed artifacts, so neither can silently go stale vs the sidecar. It also
// re-derives the expected TS rows straight from the sidecar and checks the
// committed module's DIALECT_TABLE matches — pinning the sidecar → TS transcription
// directly, not only the self-consistency of a re-run.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { DIALECT_TABLE, lookupDisposition } from "../src/generated/dialect-table.ts";

const here = dirname(fileURLToPath(import.meta.url));
const sidecarPath = resolve(here, "../../../crates/zero-migrate/dialect-support.toml");
const genScript = resolve(here, "../scripts/gen-dialect-table.mjs");
const committedTs = resolve(here, "../src/generated/dialect-table.ts");
const committedRust = resolve(here, "../../../crates/zero-migrate/tests/dialect_matrix/dialect_table.rs");

const DISPOSITIONS = new Set(["portable", "transparentDegradable", "vendor", "unsupported"]);

/** Re-read the flat `[[row]]` sidecar with the same strict subset the generator
 *  parses (blank lines, `#` comments, `[[row]]` headers, `key = "string"`). */
function parseSidecar(text: string): Array<Record<string, string>> {
  const rows: Array<Record<string, string>> = [];
  let cur: Record<string, string> | null = null;
  const lines = text.split(/\r?\n/);
  for (const raw of lines) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === "[[row]]") {
      cur = {};
      rows.push(cur);
      continue;
    }
    const m = line.match(/^([a-zA-Z][a-zA-Z0-9_]*)\s*=\s*"([^"\\]*)"\s*(#.*)?$/);
    assert.ok(m, `unsupported sidecar line: ${JSON.stringify(raw)}`);
    assert.ok(cur, `key before any [[row]] header: ${JSON.stringify(raw)}`);
    cur[m![1]] = m![2];
  }
  return rows;
}

/** The two STRUCTURAL keys of a sidecar row. Every other key is a dialect id. */
const STRUCTURAL_KEYS = new Set(["kind", "variant"]);

/** The dialect ids a sidecar row declares a disposition for, in sorted order. */
function dialectKeys(row: Record<string, string>): string[] {
  return Object.keys(row)
    .filter((k) => !STRUCTURAL_KEYS.has(k))
    .sort(compareCodeUnits);
}

const rawSidecarRows = parseSidecar(await readFile(sidecarPath, "utf8"));
const sidecarRows = rawSidecarRows.map((r) => ({
  kind: r.kind,
  variant: r.variant,
  dispositions: Object.fromEntries(dialectKeys(r).map((d) => [d, r[d]])),
}));

function sortKey(r: { kind: string; variant: string }): string {
  return `${r.kind}\0${r.variant}`;
}

/**
 * Order by UTF-16 code unit, matching the generator.
 *
 * Both sides have to use the SAME locale-independent rule or this test can accuse
 * a correctly generated file of drifting. It also makes the two orderings equal by
 * construction rather than by luck: the generator compares `kind` then `variant`,
 * this compares them joined by NUL, and those agree only because NUL sorts below
 * every other code unit - a property `localeCompare` does not promise.
 */
function compareCodeUnits(a: string, b: string): number {
  if (a < b) return -1;
  if (a > b) return 1;
  return 0;
}

test("committed dialect-table.ts matches the sidecar rows", () => {
  const expected = [...sidecarRows].sort((a, b) => compareCodeUnits(sortKey(a), sortKey(b)));
  const actual = [...DIALECT_TABLE]
    .map((r) => ({ kind: r.kind, variant: r.variant, dispositions: { ...r.dispositions } }))
    .sort((a, b) => compareCodeUnits(sortKey(a), sortKey(b)));
  assert.deepEqual(actual, expected, "generated dialect-table.ts drifted from dialect-support.toml");
});

// CENSUS FLOOR for the two scans below.
//
// Both now iterate a DISCOVERED set — `Object.keys(row.dispositions)` — rather
// than the three named fields the row used to carry. A scan over a discovered set
// FAILS OPEN: an empty `dispositions` object makes the token scan below iterate
// nothing, find nothing, and report clean, and makes the sidecar comparison above
// compare `{}` against `{}`. The three fields made that impossible by TYPE; this
// floor is what replaces the type. It pins the census on the DIALECT axis, which
// is the axis the re-keying opened.
test("every table row declares the same non-empty dialect census", () => {
  const census = [...new Set(DIALECT_TABLE.flatMap((r) => Object.keys(r.dispositions)))].sort(compareCodeUnits);
  assert.ok(census.length >= 3, `the dialect census collapsed to ${census.length} (${census.join(",")})`);
  assert.deepEqual(census, ["mysql", "postgres", "sqlite"], "the shipping dialect census changed");
  for (const row of DIALECT_TABLE) {
    assert.deepEqual(
      Object.keys(row.dispositions).sort(compareCodeUnits),
      census,
      `${row.kind}/${row.variant} declares a different dialect set from the table census`,
    );
  }
  // And the sidecar agrees, so a row cannot lose a cell on the way in either.
  for (const row of rawSidecarRows) {
    assert.deepEqual(
      dialectKeys(row),
      census,
      `sidecar row ${row.kind}/${row.variant} declares a different dialect set from the table census`,
    );
  }
});

// The sidecar's dialect KEYS are the canonical `DialectId` spellings, with no
// aliases. This used to be false: the sidecar said `pg` while every artifact it
// fed said `postgres`, so `pg → postgres` was an alias baked into the generator —
// and `DialectId`'s stated rule is "no aliases and no display names in the id".
test("sidecar dialect keys are canonical dialect ids, not aliases", () => {
  const census = [...new Set(rawSidecarRows.flatMap(dialectKeys))];
  assert.ok(census.length >= 3, `the sidecar dialect census collapsed to ${census.length}`);
  for (const id of census) {
    assert.match(id, /^[a-z][a-z0-9_]*$/, `sidecar dialect key "${id}" violates the DialectId rule`);
    assert.ok(!["pg", "postgresql", "sqlite3", "maria", "mariadb"].includes(id), `sidecar dialect key "${id}" is an alias, not a canonical id`);
  }
});

test("every dialect-table.ts disposition is a known token", () => {
  let cells = 0;
  for (const row of DIALECT_TABLE) {
    for (const d of Object.values(row.dispositions)) {
      assert.ok(DISPOSITIONS.has(d), `unknown disposition token "${d}" for ${row.kind}/${row.variant}`);
      cells += 1;
    }
  }
  // Floor: the scan above must actually have looked at something.
  assert.equal(cells, DIALECT_TABLE.length * 3, "the disposition scan did not visit every (row, dialect) cell");
});

test("lookupDisposition resolves rows and misses cleanly", () => {
  const first = DIALECT_TABLE[0];
  assert.deepEqual(lookupDisposition(first.kind, first.variant), first);
  assert.equal(lookupDisposition("noSuchOp", "base"), undefined);
});

// THE OPEN-DIALECT GATE — the reason the row is keyed by id rather than by one
// struct field per vendor.
//
// A fourth backend used to need: a new `DispositionRow` field, a new TS interface
// field, 92 new cells, a new sidecar column, a generator change, and a regenerated
// artifact. Everything but the cells was a change to CODE THE BACKEND DOES NOT OWN.
// This drives a sidecar naming a dialect the generator has never heard of through
// the COMMITTED generator — no edit — and requires it to reach both artifacts.
test("a dialect the generator has never heard of reaches both artifacts unedited", () => {
  const dir = mkdtempSync(join(tmpdir(), "zs-gendialect-open-"));
  const sidecar = join(dir, "dialect-support.toml");
  writeFileSync(
    sidecar,
    [
      "[[row]]",
      'kind = "createTable"',
      'variant = "base"',
      'postgres = "portable"',
      'sqlite = "portable"',
      'mysql = "portable"',
      'duckdb = "vendor"',
      "",
      "[[row]]",
      'kind = "createIndex"',
      'variant = "base"',
      'postgres = "portable"',
      'sqlite = "portable"',
      'mysql = "portable"',
      'duckdb = "unsupported"',
      "",
    ].join("\n"),
    "utf8",
  );
  const tsTmp = join(dir, "dialect-table.ts");
  const rustTmp = join(dir, "dialect_table.rs");
  execFileSync(process.execPath, [genScript], {
    env: { ...process.env, GEN_DIALECT_SIDECAR: sidecar, GEN_DIALECT_TS_OUT: tsTmp, GEN_DIALECT_RUST_OUT: rustTmp },
  });

  const rust = readFileSync(rustTmp, "utf8");
  const ts = readFileSync(tsTmp, "utf8");
  // The id reaches the Rust table as a `DialectId`, carrying BOTH its dispositions.
  assert.match(rust, /DialectId::new\("duckdb"\), Disposition::Vendor/, "duckdb's vendor cell is missing from the Rust table");
  assert.match(rust, /DialectId::new\("duckdb"\), Disposition::Unsupported/, "duckdb's unsupported cell is missing from the Rust table");
  // ...and the TS mirror, keyed by the same id.
  assert.match(ts, /duckdb: "vendor"/, "duckdb's vendor cell is missing from the TS mirror");
  assert.match(ts, /duckdb: "unsupported"/, "duckdb's unsupported cell is missing from the TS mirror");
  // No closed per-vendor union may be emitted: a union listing the dialects is the
  // same "core knows the vendors" shape in another spelling.
  assert.doesNotMatch(ts, /export type Dialect\b/, "the TS mirror still emits a closed Dialect union");
  // And no artifact may carry a per-vendor STRUCT FIELD for the shipping three.
  assert.doesNotMatch(rust, /pub postgres: Disposition/, "the Rust row still has a per-vendor field");
  assert.doesNotMatch(ts, /readonly postgres: Disposition/, "the TS row still has a per-vendor field");
});

test("committed generated dialect tables (TS + Rust) are up to date (regenerate + diff)", () => {
  const dir = mkdtempSync(join(tmpdir(), "zs-gendialect-"));
  const tsTmp = join(dir, "dialect-table.ts");
  const rustTmp = join(dir, "dialect_table.rs");
  execFileSync(process.execPath, [genScript], {
    env: { ...process.env, GEN_DIALECT_TS_OUT: tsTmp, GEN_DIALECT_RUST_OUT: rustTmp },
  });
  assert.equal(
    readFileSync(tsTmp, "utf8"),
    readFileSync(committedTs, "utf8"),
    "src/generated/dialect-table.ts is stale — run `pnpm --filter zero-migrate gen:dialect-table`",
  );
  assert.equal(
    readFileSync(rustTmp, "utf8"),
    readFileSync(committedRust, "utf8"),
    "crates/zero-migrate/tests/dialect_matrix/dialect_table.rs is stale — run `pnpm --filter zero-migrate gen:dialect-table`",
  );
});
