import { table, t } from "zero-migrate";

// Add the nullable destination before the following data migration fills it.
export const name = "add_full_name";

export default {
  schema() {
    table("employees").column("full_name").add({ type: t.string({ length: 512 }) });
  },
};
