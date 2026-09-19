import { table, t } from "@zeroship/migrate";

export default {
  name: "workflow_effects",
  schema() {
    const columns = () => ({
      seq: t.bigInt().identity(),
      run_id: t.text().required(),
      step_name: t.text().required(),
    });
    table("workflow_e2e_side_effects").create({ columns: columns() });
    table("workflow_e2e_effect_attempts").create({
      columns: { ...columns(), idempotency_key: t.text().required() },
    });
    table("workflow_e2e_effect_commits").create({
      columns: { ...columns(), idempotency_key: t.text().required().unique() },
    });
  },
};
