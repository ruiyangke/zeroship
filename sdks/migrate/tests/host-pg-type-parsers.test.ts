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

/** Marker every poisoned parser stamps onto its output. */
const POISON = "POISONED:";

/** A `pg.types` stand-in poisoned for EVERY oid, as a global `setTypeParser`
 *  would leave the ones it touched.
 *
 *  Two properties matter in the shape of this. Poisoning every oid rather than a
 *  list asserts the property the driver needs - that no parser it relies on is
 *  reachable through `setTypeParser` - instead of a set of instances an author
 *  happened to suspect. And the output is MARKED rather than identity, so a test
 *  can tell "the driver pinned this" apart from "the stand-in was never reached".
 *  An identity poison cannot: a real text parser is identity too, so an assertion
 *  against it passes whether or not the poison is wired in at all. */
function poisonedPgModule() {
  return {
    types: {
      getTypeParser(): (value: string) => unknown {
        return (value: string) => `${POISON}${value}`;
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

test("name[] decodes without consulting the global parser map", () => {
  // pg-types registers array parsers for 1000, 1009, 1015 and 1016, but not for
  // 1003. Catalog introspection returns name[] from array_agg(attname), so
  // without a parser the raw literal crosses and the seam's Vec<String> decode
  // fails. Verified against pg-types/lib/textParsers.js rather than assumed.
  //
  // The module here poisons EVERY oid, including text[]. Borrowing the text[]
  // parser would be a live read of this same map, so a shadow written that way
  // would fail here - which is the point.
  const OID_NAME_ARRAY = 1003;
  const scoped = __testing.connectionScopedTypes(poisonedPgModule() as never);
  assert.deepEqual(
    scoped.getTypeParser(OID_NAME_ARRAY)("{a,b}"),
    ["a", "b"],
    "name[] must decode as an array even when every global parser is poisoned",
  );
});

test("the array parser handles quoting, escapes, nulls and the empty array", () => {
  const parse = __testing.parsePgTextArray;

  assert.deepEqual(parse("{}"), [], "empty array");
  assert.deepEqual(parse("{a,b}"), ["a", "b"], "plain elements");
  assert.deepEqual(parse('{"a,b",c}'), ["a,b", "c"], "a quoted comma is not a separator");
  assert.deepEqual(parse('{"say \\"hi\\""}'), ['say "hi"'], "escaped quotes inside an element");
  assert.deepEqual(parse("{a,NULL,b}"), ["a", null, "b"], "unquoted NULL is a null element");
  assert.deepEqual(parse('{"NULL"}'), ["NULL"], "quoted NULL is the literal string");
  assert.deepEqual(parse('{"a\\\\b"}'), ["a\\b"], "escaped backslash");
  assert.throws(() => parse("a,b"), /malformed/, "a literal without braces is rejected");
});

test("a nested array is rejected rather than flattened into garbage", () => {
  // The seam carries a one-dimensional array, so a nested value has no faithful
  // form here. Treating the inner braces as ordinary characters yields elements
  // like "{a" and "b}", which the cell fold would happily pass on as strings.
  // Failing the verb is the only honest option; silently mangling is not.
  assert.throws(
    () => __testing.parsePgTextArray("{{a,b},{c}}"),
    /nested/,
    "a nested literal must throw, not decode to broken elements",
  );

  // A dimension prefix has no faithful one-dimensional reading either.
  assert.throws(
    () => __testing.parsePgTextArray("[1:2]={a,b}"),
    /malformed/,
    "a dimension-prefixed literal must throw",
  );
});

test("exact-integer pinning still holds and other oids still delegate", () => {
  const scoped = __testing.connectionScopedTypes(poisonedPgModule() as never);

  // int8 stays a verbatim string so values above 2^53 keep their exact digits.
  assert.equal(scoped.getTypeParser(OID_INT8)("9007199254740993"), "9007199254740993");

  // CONTROL: an oid the driver does NOT pin must reach the poisoned stand-in.
  // Without this the whole file could pass while the stand-in was never wired in,
  // and every "the pin survives poisoning" assertion above would be vacuous. This
  // is the assertion that makes the others mean something.
  assert.equal(
    scoped.getTypeParser(OID_TEXT)("hello"),
    `${POISON}hello`,
    "an unpinned oid must be served by the module, proving the poison is reachable",
  );
});
