// Connection-scoped type parsers must be immune to a global `setTypeParser`
// override, including for bool.
//
// `pg.types.setTypeParser` writes a process-wide mutable map. Any dependency can
// call it at import time - an ORM wanting raw 't'/'f' is the usual reason - and
// the driver's own parser table falls through to that map for every oid it does
// not pin. Bool was left unpinned on the reasoning that node-pg's default already
// returns a boolean, which is true right up until something overrides it.
//
// The consequence is silent rather than loud: a poisoned parser delivers a
// string, `valueToCell` still classifies the column as bool from its oid, and
// `Boolean("f")` is true. Every false becomes true with nothing raised. That
// value reaches precondition guards on destructive migrations and the
// catalog-drift comparison.

import assert from "node:assert/strict";
import { test } from "node:test";

import { __testing } from "../src/host/driver-pg.js";

const OID_BOOL = 16;
const OID_INT8 = 20;
const OID_TEXT = 25;

/** A `pg.types` stand-in whose bool parser has been poisoned, as a global
 *  `setTypeParser(16, ...)` would leave it. */
function poisonedPgModule() {
  return {
    types: {
      getTypeParser(oid: number): (value: string) => unknown {
        if (oid === OID_BOOL) return (value: string) => value; // raw 't' / 'f'
        if (oid === OID_INT8) return (value: string) => value;
        return (value: string) => value;
      },
    },
  };
}

test("bool parsing survives a poisoned global type parser", () => {
  const scoped = __testing.connectionScopedTypes(poisonedPgModule() as never);
  const parseBool = scoped.getTypeParser(OID_BOOL);

  assert.equal(parseBool("f"), false, "'f' must decode as false, not as a truthy string");
  assert.equal(parseBool("t"), true, "'t' must decode as true");
  assert.equal(typeof parseBool("f"), "boolean", "the scoped parser must yield a boolean");
});

test("exact-integer pinning still holds and other oids still delegate", () => {
  const scoped = __testing.connectionScopedTypes(poisonedPgModule() as never);

  // int8 stays a verbatim string so values above 2^53 keep their exact digits.
  assert.equal(scoped.getTypeParser(OID_INT8)("9007199254740993"), "9007199254740993");

  // An oid the driver does not pin is still served by the module's own parser.
  assert.equal(scoped.getTypeParser(OID_TEXT)("hello"), "hello");
});
