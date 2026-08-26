// Every authoring entry point refuses an unknown key, and none of them refuses a
// valid one.
//
// The refusal itself landed across six commits (`723cf004` .. `dedad1ad`) after a
// misspelled key was found to be silently discarded: `create({ fk })` recorded no
// foreign key, `create({ ifNotExist })` recorded no existence guard and then failed
// on the re-run the guard existed for, and `primaryKey().drop({ dropIdentityFrm })`
// skipped an identity transition while reporting success. TypeScript rejected all
// of them; `apply` loads migrations through tsx WITHOUT typechecking, so nothing
// objected at the moment it mattered.
//
// WHAT THIS FILE IS FOR IS THE NEXT ENTRY POINT, NOT THE ONES ALREADY FIXED.
// A guard is one line and easy to omit when adding a method, and omitting it fails
// nothing: the new method works, the suite stays green, and the silence returns for
// that call only. Each case below is cheap; the value is that the list has to grow
// when the surface does.
//
// Two arms per entry point, and both matter:
//
//   VALID    the supported shape still records. Without this, a guard with a
//            wrong key list - a real risk, since each list mirrors an interface
//            by hand - would pass a refusal-only suite while breaking authors.
//   UNKNOWN  the same call plus one bogus key is refused, and the message names
//            the key. `zzUnknownKey` is used everywhere so a failure reads the
//            same way whichever entry point produced it.
//
// The DSL suite already caught two things a hand probe missed while this was being
// built: an index-drop test that pinned the OLD silent behaviour, and a generic
// guard shadowing `backfill`'s specific "cursorColumn was removed" message. Both
// are why the refusal is asserted by message content rather than by "it threw".

import assert from "node:assert/strict";
import { test } from "node:test";

import { __abort, __begin, __drain } from "../src/ops.js";
import { domain, enumType, extension, role, schema, sequence, t, table, view } from "../src/index.js";

/** The one bogus key, so every failure message reads identically. */
const UNKNOWN = { zzUnknownKey: 1 };

const COLUMNS = { id: t.int().notNull(), v: t.int().notNull() };
const BASE = { columns: COLUMNS, primaryKey: ["id"] as string[] };

/** Re-declared per case: recording is eager, so each case needs its own table. */
function seed(): void {
  table("a").create(BASE);
}

/**
 * `[name, call]` where `call` takes the extra keys to splice into the args object.
 * Passing `{}` must record; passing `UNKNOWN` must be refused.
 */
const ENTRY_POINTS: ReadonlyArray<readonly [string, (extra: object) => void]> = [
  ["table().create", (x) => table("a").create({ ...BASE, ...x })],
  ["table().rename", (x) => { seed(); table("a").rename({ to: "b", ...x }); }],
  ["table().drop", (x) => { seed(); table("a").drop({ ...x }); }],
  ["table().setOptions", (x) => { seed(); table("a").setOptions({ softDelete: true, ...x }); }],
  ["table().comment", (x) => { seed(); table("a").comment("c", { ...x }); }],

  ["column().add", (x) => { seed(); table("a").column("w").add({ type: t.int(), ...x }); }],
  ["column().drop", (x) => { seed(); table("a").column("v").drop({ ...x }); }],
  ["column().rename", (x) => { seed(); table("a").column("v").rename({ to: "w", type: t.int().notNull(), ...x }); }],
  ["column().setType", (x) => { seed(); table("a").column("v").setType({ to: t.text(), ...x }); }],
  ["column().setNotNull", (x) => { seed(); table("a").column("v").setNotNull({ ...x }); }],
  ["column().dropNotNull", (x) => { seed(); table("a").column("v").dropNotNull({ ...x }); }],
  ["column().dropDefault", (x) => { seed(); table("a").column("v").dropDefault({ ...x }); }],

  ["index().add", (x) => { seed(); table("a").index("ix").add({ on: [{ column: "v" }], ...x }); }],
  ["index().drop", (x) => { seed(); table("a").index("ix").drop({ ...x }); }],

  ["primaryKey().add", (x) => { seed(); table("a").primaryKey().add({ columns: ["id"], ...x }); }],
  ["primaryKey().drop", (x) => { seed(); table("a").primaryKey().drop({ expectedColumns: ["id"], ...x }); }],

  ["table().insert", (x) => { seed(); table("a").insert({ rows: [{ id: 1, v: 1 }], ...x }); }],
  ["table().update", (x) => { seed(); table("a").update({ set: { v: 1 }, where: (c) => c("id").gt(0), ...x }); }],
  ["table().delete", (x) => { seed(); table("a").delete({ where: (c) => c("id").gt(0), ...x }); }],
  ["table().backfill", (x) => {
    seed();
    table("a").backfill({
      set: { v: 1 },
      where: (c) => c("id").gt(0),
      cursorColumns: ["id"],
      cursorStability: { mode: "externalInvariant", name: "a_id" },
      batchSize: 2,
      ...x,
    });
  }],

  ["partition().drop", (x) => { seed(); table("a").partition("p").drop({ ...x }); }],
  ["policy().drop", (x) => { seed(); table("a").policy("p").drop({ ...x }); }],
  ["trigger().drop", (x) => { seed(); table("a").trigger("tg").drop({ ...x }); }],

  ["view().create", (x) => view("v").create({ as: { raw: "SELECT 1" }, ...x })],
  ["view().drop", (x) => view("v").drop({ ...x })],

  ["schema().create", (x) => schema("s").create({ ...x })],
  ["schema().drop", (x) => schema("s").drop({ ...x })],
  ["extension().create", (x) => extension("citext").create({ ...x })],
  ["extension().drop", (x) => extension("citext").drop({ ...x })],
  ["role().create", (x) => role("r").create({ login: true, ...x })],
  ["role().setOptions", (x) => role("r").setOptions({ setSearchPath: ["public"], ...x })],
  ["role().drop", (x) => role("r").drop({ ...x })],
  ["sequence().create", (x) => sequence("q").create({ increment: 2, ...x })],
  ["sequence().alter", (x) => sequence("q").alter({ restart: 5, ...x })],
  ["sequence().drop", (x) => sequence("q").drop({ ...x })],
  ["enumType().create", (x) => enumType("e").create({ values: ["a"], ...x })],
  ["enumType().drop", (x) => enumType("e").drop({ ...x })],
  ["domain().create", (x) => domain("d").create({ as: t.int(), ...x })],
  ["domain().drop", (x) => domain("d").drop({ ...x })],
] as const;

/** Trigger-body statements, whose builder is a separate nested surface. */
const TRIGGER_STATEMENTS: ReadonlyArray<readonly [string, (extra: object) => void]> = [
  ["b.raise", (x) => triggerBody((b) => [b.raise({ level: "abort", message: "m", ...x })])],
  ["b.insert", (x) => triggerBody((b) => [b.insert({ table: "a", rows: [{ id: 1 }], ...x })])],
  ["b.update", (x) => triggerBody((b) => [b.update({ table: "a", set: { id: 1 }, where: (c: any) => c("id").gt(0), ...x })])],
  ["b.delete", (x) => triggerBody((b) => [b.delete({ table: "a", where: (c: any) => c("id").gt(0), ...x })])],
] as const;

function triggerBody(body: (b: any) => unknown[]): void {
  table("a").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
  table("a").trigger("tg").create({ timing: "before", events: ["insert"], body } as never);
}

/** Runs `call`, always draining so one case cannot leak ops into the next. */
function attempt(call: (extra: object) => void, extra: object): Error | null {
  __begin();
  try {
    call(extra);
    __drain();
    return null;
  } catch (error) {
    __abort();
    return error as Error;
  }
}

for (const [name, call] of [...ENTRY_POINTS, ...TRIGGER_STATEMENTS]) {
  test(`${name} records its supported shape`, () => {
    const failure = attempt(call, {});
    assert.equal(
      failure,
      null,
      `the supported shape must still record - a guard whose key list is missing a ` +
        `real option breaks authors while a refusal-only test stays green: ${failure?.message}`,
    );
  });

  test(`${name} refuses an unknown key`, () => {
    const failure = attempt(call, UNKNOWN);
    assert.ok(
      failure,
      `an unknown key must be refused, not silently discarded - that is how a ` +
        `misspelled option loses a constraint, a guard, or a data step`,
    );
    assert.match(
      failure.message,
      /does not accept "zzUnknownKey"/,
      `the refusal must name the offending key, since the real cause is always a ` +
        `near-miss spelling: ${failure.message}`,
    );
  });
}

// Non-vacuity: the list above is only worth what it covers, and a silent shrink
// (a bad merge, an accidental truncation) would leave a green suite asserting
// almost nothing. Raise this deliberately when the surface grows.
test("the entry-point list still covers the whole authoring surface", () => {
  const total = ENTRY_POINTS.length + TRIGGER_STATEMENTS.length;
  assert.ok(
    total >= 41,
    `only ${total} entry points are covered; the list has shrunk, so this suite is ` +
      `no longer checking what its name claims`,
  );
});
