// A backend namespace authored on `create()` reaches the recorded op.
//
// The DSL is the FIRST link in the chain that ends at a `WITH (fillfactor='85')` in the
// database. Every later link — the wire op, the fold, the snapshot, the vendor renderer —
// already has its own test in the Rust suite. What only this file can check is that the
// authoring surface EMITS anything at all: before it did, `postgres: { fillfactor: 85 }`
// type-checked against the generated namespace map and was then dropped on the floor,
// which is a fail-open the author never sees.

import assert from "node:assert/strict";
import { test } from "node:test";

import { t, table } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

/** Drive the ambient recorder and hand back the recorded ops. */
function record(fn: () => void): Record<string, unknown>[] {
  __begin();
  fn();
  return __drain() as unknown as Record<string, unknown>[];
}

test("a backend namespace on create() is flattened onto the op's attributes", () => {
  const ops = record(() =>
    // Cast because THIS package declares no namespaces: `VendorAttributeNamespaces` is
    // empty here by design, and `postgres` becomes a real key only once
    // `zero-migrate-postgres` is installed and merges its declaration in. The runtime
    // behaviour under test is identical either way.
    table("widgets").create({
      columns: { id: t.int() },
      postgres: { fillfactor: 85 },
    } as never),
  );
  assert.deepEqual(ops[0].attributes, { "postgres.fillfactor": 85 });
});

test("several backends' namespaces coexist on one portable table", () => {
  const ops = record(() =>
    table("widgets").create({
      columns: { id: t.int() },
      postgres: { fillfactor: 85 },
      mysql: { engine: "InnoDB", row_format: "DYNAMIC" },
      sqlite: { strict: true },
    } as never),
  );
  // All three survive together. Each backend's renderer selects its own namespace, so a
  // table carrying options for three engines stays deployable to all of them — which is
  // the property that makes a flat `<dialect>.<name>` key space worth having.
  assert.deepEqual(ops[0].attributes, {
    "postgres.fillfactor": 85,
    "mysql.engine": "InnoDB",
    "mysql.row_format": "DYNAMIC",
    "sqlite.strict": true,
  });
});

test("a create with no namespace records no attributes key at all", () => {
  const ops = record(() => table("plain").create({ columns: { id: t.int() } }));
  // ABSENT, not an empty object. The Rust field skips serializing an empty map, so an
  // empty `{}` here would change the wire bytes of every existing migration and break
  // every pinned checksum.
  assert.equal(Object.prototype.hasOwnProperty.call(ops[0], "attributes"), false);
});

test("an undefined leaf is omitted rather than recorded as null", () => {
  const ops = record(() =>
    table("widgets").create({
      columns: { id: t.int() },
      postgres: { fillfactor: undefined },
    } as never),
  );
  // `{ fillfactor: undefined }` is how an optional field reads when a caller spreads a
  // config object. It means "not set", and must not become a present key carrying null —
  // which the vocabulary would then refuse as a wrong-typed value.
  assert.equal(Object.prototype.hasOwnProperty.call(ops[0], "attributes"), false);
});

test("a misspelled portable key is still refused when it is not an object", () => {
  // The unknown-key gate had to loosen to admit open-ended backend namespaces. This pins
  // that it did not become a no-op: a scalar-valued unknown key cannot be a namespace, so
  // it is still caught here with the same structured error as before.
  assert.throws(
    () =>
      record(() =>
        table("widgets").create({
          columns: { id: t.int() },
          ifNotExist: true,
        } as never),
      ),
    /does not accept "ifNotExist"/,
  );
});
