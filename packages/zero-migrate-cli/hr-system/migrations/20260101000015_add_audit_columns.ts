import { table, t } from "zero-migrate";

// Add nullable audit timestamps to the two mutable core tables. Final schema
// evolution step; a clean additive change.
export const name = "add_audit_columns";

export default {
  up() {
    table("departments").column("updated_at").add({ type: t.timestamp() });
    table("employees").column("updated_at").add({ type: t.timestamp() });
  },
};
