import assert from "node:assert/strict";

export type Usage = Record<string, number>;

export function assertUsage(usage: Usage, requests: number, rows: number) {
  assert(requests > 0 && rows > 0, "The workload must perform database writes");
  assert.equal(usage.requests, requests, "Request usage must match browser and readiness requests");
  assert.equal(usage.db_writes, rows, "Write usage must match the physical table");
  assert.equal(usage.db_rows_written, rows, "Row usage must match the physical table");
  assert(usage.db_reads >= rows && usage.db_reads <= requests, "Read usage must match successful writes and attempted requests");
  for (const metric of ["requests", "cpu_us", "wall_us", "egress_bytes"]) {
    assert(usage[metric] > 0, `${metric} must reach the usage aggregates`);
  }
}
