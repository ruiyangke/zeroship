// op.* migration fixture — collection runtime options that are not recoverable
// from physical catalog state. Covers create-time runtimeOptions, a runtime-
// visible compound index, and a later metadata-only setTableOptions patch.
import { table, t } from "@zeroship/migrate";

export const name = "runtime_options";

export function up() {
  table("posts").create({
    columns: {
      title: t.text().notNull(),
      author_id: t.uuid().notNull(),
      status: t.text().notNull().default("draft"),
    },
    softDelete: true,
    versioning: true,
    strictness: "lenient",
  });

  table("posts").index("posts_author_status_idx").add({
    columns: ["author_id", "status"],
  });

  table("posts").withVersioning(false);
  table("posts").strictness("off");
}
