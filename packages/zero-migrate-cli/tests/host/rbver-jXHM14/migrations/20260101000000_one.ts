import { table, t } from "zero-migrate";
export const name = "one";
export default { schema() {
  table("rbv_one").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
} };
