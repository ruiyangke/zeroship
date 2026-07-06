// op.* migration fixture — Migration-first P2a: the DECLARED-ONLY column facets
// the recorder now captures on the wire `IrColumn` (design
// 2026-06-25-p2-gen-types-type-source.md §2b). Proves `t.id({ prefix })` records
// `idPrefix` (was dropped pre-P2a — migrate_ops.js:252) and
// `t.vector({ dimensions, metric })` records `vectorMetric` (was dropped — migrate_ops.js:270), and
// that the JS↔Rust value-checksum round-trip agrees on the new optional fields.
//
// A plain column (no facet) is unchanged on the wire — the fixture mixes facet
// and non-facet columns so the byte-identity golden also covers the absent case.
import { table, t } from "@zeroship/migrate";

export default {
  name: "p2a_facets",

  up() {
    table("posts").create({
      columns: {
        // t.id({ prefix }) → IrColumn.idPrefix on the wire (a declared-only,
        // uncatalogable typed-id brand).
        id: t.id({ prefix: "post" }),
        title: t.text().notNull(),
        // t.vector({ dimensions, metric }) → IrColumn.vectorMetric (the closed cosine|l2|
        // innerProduct set) — the other declared-only hint.
        embedding: t.vector({ dimensions: 1536, metric: "cosine" }),
        // A plain vector with NO metric: the facet is OMITTED on the wire.
        secondary_embedding: t.vector({ dimensions: 768 }),
      },
    });
  },
};
