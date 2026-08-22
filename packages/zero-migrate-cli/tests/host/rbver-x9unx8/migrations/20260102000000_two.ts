import { table, t } from "zero-migrate";
export const name = "two";
export default { schema() {
  table("rbv_two").create({ columns: { id: t.int().notNull() }, primaryKey: ["id"] });
} };
