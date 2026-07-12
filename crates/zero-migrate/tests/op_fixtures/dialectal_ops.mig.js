// op-level dialect() fixture: a whole PostgreSQL-only op is present on PG and
// absent on SQLite/MySQL.
import { dialect, table, t } from "zero-migrate";

export default {
  name: "dialectal_ops",

  up() {
    table("docs").create({
      columns: {
        id: t.uuid().primaryKey(),
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
