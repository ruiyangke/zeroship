// Drift guard for the generated dialect table (DSL redesign Phase 0, S0.1).
//
// The single source is `crates/zeroship-migrate/dialect-support.toml`; the
// generator (`scripts/gen-dialect-table.mjs`) emits BOTH the committed TS mirror
// (`src/generated/dialect-table.ts`) and the committed Rust table
// (`crates/zeroship-migrate/src/model/dialect_table.rs`). This test is the
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
const sidecarPath = resolve(here, "../../../third_party/zero-migrate/crates/zero-migrate/dialect-support.toml");
const genScript = resolve(here, "../scripts/gen-dialect-table.mjs");
const committedTs = resolve(here, "../src/generated/dialect-table.ts");
const committedRust = resolve(here, "../../../third_party/zero-migrate/crates/zero-migrate/src/model/dialect_table.rs");

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

test("committed dialect-table.ts matches the sidecar rows", () => {
  const expected = [...sidecarRows].sort((a, b) => sortKey(a).localeCompare(sortKey(b)));
  const actual = [...DIALECT_TABLE]
    .map((r) => ({ kind: r.kind, variant: r.variant, postgres: r.postgres, sqlite: r.sqlite, mysql: r.mysql }))
    .sort((a, b) => sortKey(a).localeCompare(sortKey(b)));
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

// Only the TS mirror is asserted here, and that is a narrowing.
//
// This used to also require our generator to reproduce the engine's committed
// `dialect_table.rs` byte for byte. That assertion was false - our generator has
// fallen behind the submodule's own and emits a pre-rename source path, a process
// marker, and no `#[rustfmt::skip]` - and it was not ours to make: the file lives
// in a vendored submodule we do not commit to, produced by that submodule's
// generator. Worse, the remedy it named was to run OUR generator, which would
// have overwritten that file with the inferior output and then let this test pass
// by comparing our generator against itself.
//
// The engine's copy is gated on the engine's side. What zeroship owns, and what
// this pins, is that our TS mirror matches the sidecar.
test("the committed dialect-table.ts is up to date (regenerate + diff)", () => {
  const dir = mkdtempSync(join(tmpdir(), "zs-gendialect-"));
  const tsTmp = join(dir, "dialect-table.ts");
  execFileSync(process.execPath, [genScript], {
    env: { ...process.env, GEN_DIALECT_TS_OUT: tsTmp },
  });
  assert.equal(
    readFileSync(tsTmp, "utf8"),
    readFileSync(committedTs, "utf8"),
    "src/generated/dialect-table.ts is stale — run `pnpm --filter @zeroship/migrate gen:dialect-table`",
  );
});
