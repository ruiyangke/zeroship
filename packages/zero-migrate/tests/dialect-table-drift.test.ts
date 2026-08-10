// Drift guard for the generated dialect table.
//
// The single source is `crates/zero-migrate/dialect-support.toml`; the
// generator (`scripts/gen-dialect-table.mjs`) emits BOTH the committed TS mirror
// (`src/generated/dialect-table.ts`) and the committed Rust table
// (`crates/zero-migrate/src/model/dialect_table.rs`). This test is the
// "regenerate + diff" freshness gate (the same shape as ir-types-drift's enums
// gate): re-run the generator into temp files and assert byte-equality with both
// committed artifacts, so neither can silently go stale vs the sidecar. It also
// re-derives the expected TS rows straight from the sidecar and checks the
// committed module's DIALECT_TABLE matches — pinning the sidecar → TS transcription
// directly, not only the self-consistency of a re-run.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
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
const committedRust = resolve(here, "../../../crates/zero-migrate/src/model/dialect_table.rs");

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

const sidecarRows = parseSidecar(await readFile(sidecarPath, "utf8")).map((r) => ({
  kind: r.kind,
  variant: r.variant,
  postgres: r.pg,
  sqlite: r.sqlite,
  mysql: r.mysql,
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
    .map((r) => ({ kind: r.kind, variant: r.variant, postgres: r.postgres, sqlite: r.sqlite, mysql: r.mysql }))
    .sort((a, b) => compareCodeUnits(sortKey(a), sortKey(b)));
  assert.deepEqual(actual, expected, "generated dialect-table.ts drifted from dialect-support.toml");
});

test("every dialect-table.ts disposition is a known token", () => {
  for (const row of DIALECT_TABLE) {
    for (const d of [row.postgres, row.sqlite, row.mysql]) {
      assert.ok(DISPOSITIONS.has(d), `unknown disposition token "${d}" for ${row.kind}/${row.variant}`);
    }
  }
});

test("lookupDisposition resolves rows and misses cleanly", () => {
  const first = DIALECT_TABLE[0];
  assert.deepEqual(lookupDisposition(first.kind, first.variant), first);
  assert.equal(lookupDisposition("noSuchOp", "base"), undefined);
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
    "crates/zero-migrate/src/model/dialect_table.rs is stale — run `pnpm --filter zero-migrate gen:dialect-table`",
  );
});
