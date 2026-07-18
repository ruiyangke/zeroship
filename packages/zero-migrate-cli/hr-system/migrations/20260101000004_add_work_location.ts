import { table, t } from "zero-migrate";

// Remote-work rollout: add a nullable work_location to employees. A plain
// additive column change, portable across all three dialects.
export const name = "add_work_location";

export default {
  up() {
    table("employees").column("work_location").add({ type: t.text() });
  },
};
