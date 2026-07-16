// A multi-op golden migration for the cross-language e2e parity oracle (redesign
// step 6b): createTable + addColumn + createIndex — three op kinds in one artifact,
// so the apply exercises table DDL, an ALTER-add, and an index create over the real
// `pg` driver, and the journal ordering / checksum-anchor can be asserted end to end.
import { table, t } from "zero-migrate";

export const name = "create_gadgets";
export default {
  up() {
    // 1. createTable — author columns (system shape folded in the addon lower).
    table("gadgets").create({
      columns: {
        sku: t.text().notNull(),
        kind: t.text().notNull().default("widget"),
      },
    });
    // 2. addColumn — an ALTER add on the same table.
    table("gadgets").column("price").add({ type: t.int() });
    // 3. createIndex — a plain btree index on the authored `sku` column.
    table("gadgets").index("gadgets_sku_idx").add({ on: ["sku"] });
  },
};
