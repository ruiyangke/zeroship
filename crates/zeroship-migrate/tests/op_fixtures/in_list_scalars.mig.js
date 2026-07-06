import { table, t } from "@zeroship/migrate";

export function up() {
  table("scalar_membership").create({
    columns: {
      http_status: t.int().notNull(),
      enabled: t.boolean().notNull(),
    },
    checks: [
      { name: "http_status_known", expr: (c) => c("http_status").in([200, 404, 500]) },
      { name: "enabled_known", expr: (c) => c("enabled").in([true, false]) },
    ],
  });
}
