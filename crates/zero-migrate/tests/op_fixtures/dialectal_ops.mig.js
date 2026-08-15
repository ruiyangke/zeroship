// op-level dialect() fixture: a whole PostgreSQL-only op is present on PG and
// absent on SQLite/MySQL.
//
// Like the other confined-platform fixtures, this one omits `id`: policy injects the
// internal platform key and pins the primary key, and an author column of that name
// is a collision the resolver refuses.
import { dialect, table, t } from "zero-migrate";

export default {
  name: "dialectal_ops",

  schema() {
    table("docs").create({
      columns: {
        embedding: t.vector({ dimensions: 3, metric: "cosine" }),
      },
    });

    dialect({
      pg: () => table("docs").index("docs_embedding_hnsw_idx").add({
        on: ["embedding"],
        using: "hnsw",
      }),
    });
  },
};
