import { table, t, ids } from "zero-migrate";

// Positions/titles use a public ULID key. Title uniqueness is a unique index
// added later (portable across all dialects).
export const name = "create_positions";

export default {
  up() {
    table("positions").create({
      columns: {
        id: ids.ulid().primaryKey(),
        title: t.text().notNull(),
        department_scope: t.char({ length: 8 }),
        is_leadership: t.boolean().notNull().default(false),
      },
    });
  },
};
