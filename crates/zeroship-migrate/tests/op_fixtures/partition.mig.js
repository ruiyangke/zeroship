// op.* migration fixture — partitioned-table authoring surface. Covers
// createTable.partitionBy, createPartition range/default bounds, detach/drop
// partition lifecycle ops, and PG partition-index facets (BRIN/INCLUDE/WITH/ONLY).
import { dropPartition, p, partition, table, t } from "@zeroship/migrate";

export const name = "partition";

export function up() {
  table("events").create({
    columns: {
      ts: t.timestamp(),
      tenant_id: t.text(),
    },
    partitionBy: p.range(["ts"]),
  });

  partition("events_2026_05").of("events").forValues({
    from: ["2026-05-01T00:00:00Z"],
    to: ["2026-06-01T00:00:00Z"],
  });
  partition("events_default").of("events").asDefault();

  table("events").detachPartition("events_2026_05", { concurrently: true });
  dropPartition("events_2026_05", { cascade: true });

  table("events")
    .index("events_ts_brin_idx")
    .using("brin")
    .include(["tenant_id"])
    .with({ pagesPerRange: 32 })
    .only()
    .add({ columns: ["ts"] });
}
