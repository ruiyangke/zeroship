import { table, t, ids } from "zero-migrate";

// Positions/titles use a public ULID key. Title uniqueness is a unique index
// added later (portable across all dialects).
export const name = "create_positions";

export default {
  schema() {
    table("positions").create({
      columns: {
        id: ids.ulid().primaryKey(),
        // `title` has a unique index (added later), so it must be a bounded
        // `t.string` (VARCHAR) — MySQL cannot index unbounded `t.text()`.
        title: t.string({ length: 255 }).notNull(),
        department_scope: t.char({ length: 8 }),
        is_leadership: t.boolean().notNull().default(false),
      },
    });
  },
};
