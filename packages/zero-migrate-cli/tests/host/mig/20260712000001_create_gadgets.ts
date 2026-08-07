// A three-op golden migration: createTable + addColumn + createIndex in a single
// artifact, so one apply covers table DDL, an ALTER-add, and an index create over a
// real driver.
//
// It exists so the e2e suites have an artifact whose JOURNAL ORDERING and CHECKSUM
// ANCHOR are both assertable end to end: the three steps must be journaled in
// declaration order, re-applying the identical artifact must fold the SAME anchor,
// and any edit to the op list must fold a different one.
import { table, t } from "zero-migrate";

export const name = "create_gadgets";
export default {
  up() {
    // 1. createTable — author columns (system shape folded in the addon lower).
    table("gadgets").create({
      columns: {
        // `sku` is indexed below and `kind` carries a create-time default, so both
        // are bounded `t.string({ length })`: MySQL refuses a key over an unbounded
        // TEXT column with no prefix length, and refuses a literal DEFAULT on one.
        sku: t.string({ length: 64 }).notNull(),
        kind: t.string({ length: 32 }).notNull().default("widget"),
      },
    });
    // 2. addColumn — an ALTER add on the same table.
    table("gadgets").column("price").add({ type: t.int() });
    // 3. createIndex — a plain btree index on the authored `sku` column.
    table("gadgets").index("gadgets_sku_idx").add({ on: ["sku"] });
  },
};
