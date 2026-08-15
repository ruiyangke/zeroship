// op.* migration fixture — the DECLARED-ONLY column facets
// the recorder captures on the wire `IrColumn`. Proves
// `t.vector({ dimensions, metric })` records `vectorMetric` (previously dropped), and
// that the JS↔Rust value-checksum round-trip agrees on the optional facet.
//
// This confined-platform fixture intentionally omits an authored `id`: policy
// injects its internal text/base62 UUIDv7 platform id. That value is not a TypeID.
//
// A plain column (no facet) is unchanged on the wire — the fixture mixes facet
// and non-facet columns so the byte-identity golden also covers the absent case.
import { table, t } from "zero-migrate";

export default {
  name: "p2a_facets",

  schema() {
    table("posts").create({
      columns: {
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
