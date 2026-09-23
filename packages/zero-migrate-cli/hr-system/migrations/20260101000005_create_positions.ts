import { table, t } from "@zeroship/migrate";

// Positions/titles carry an explicit public key supplied by the seed. Title
// uniqueness is a unique index added later (portable across all dialects).
export const name = "create_positions";

export default {
  schema() {
    table("positions").create({
      columns: {
        // `id` is the primary key and the target of a foreign key, so it must be
        // a bounded `t.string` (VARCHAR) — MySQL cannot index unbounded `t.text()`.
        id: t.string({ length: 26 }).primaryKey(),
        // `title` has a unique index (added later), so it must be a bounded
        // `t.string` (VARCHAR) — MySQL cannot index unbounded `t.text()`.
        title: t.string({ length: 255 }).required(),
        department_scope: t.char({ length: 8 }),
        is_leadership: t.boolean().required().default(false),
      },
    });
  },
};
