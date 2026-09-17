import { table, t } from "@zeroship/migrate";

export function schema() {
  table("scalar_membership").create({
    columns: {
      http_status: t.int().required(),
      enabled: t.boolean().required(),
    },
    checks: [
      { name: "http_status_known", expr: (col) => col("http_status").in([200, 404, 500]) },
      { name: "enabled_known", expr: (col) => col("enabled").in([true, false]) },
    ],
  });
}
