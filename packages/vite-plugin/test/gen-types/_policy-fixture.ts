import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { parse } from "smol-toml";

type Assignment = { by: string; on: string };
type InjectRule = {
  columns: Array<{ name: string; assign: Assignment }>;
  primary_key: string[];
};
type Field = { assign?: Assignment; writable?: boolean; primaryKey?: boolean };

const policy = parse(readFileSync(
  new URL("../../../../policies/confined-system-shape.inject.toml", import.meta.url),
  "utf8",
));
assert.ok(Array.isArray(policy.inject) && policy.inject.length > 0);
const rules = policy.inject as unknown as InjectRule[];

/** Compare generated fields with the policy source, independently of its TS mirror. */
export function assertPolicyColumns(fields: Record<string, Field>): void {
  for (const rule of rules) {
    assert.ok(rule.columns.length > 0, "policy fixture must declare columns");
    for (const column of rule.columns) {
      const field = fields[column.name];
      assert.ok(field, `${column.name} is declared`);
      assert.ok(column.assign, `${column.name} has a policy assignment`);
      assert.deepEqual(field.assign, column.assign, `${column.name} preserves its generator`);
      assert.equal(field.writable, false, `${column.name} is assigned, not caller-writable`);
      assert.equal(field.primaryKey === true, rule.primary_key.includes(column.name));
    }
  }
}
