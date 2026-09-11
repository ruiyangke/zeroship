import { strict as assert } from "node:assert";
import { test } from "node:test";
import { Collection, eq, count, gt, type Row } from "../dist/index.js";
import { readFrom, type NativeDb } from "../dist/internal.js";

function fixture() {
  const calls: Record<string, unknown>[] = [];
  const native = { collection: () => ({ read: async (input: Record<string, unknown>) => {
    calls.push(input);
    return [{ order: { customer_id: "c", note: "paid" }, customer: null }];
  } }) } as unknown as NativeDb;
  const orders = new Collection<{ customerId: string; note: string }>("orders", { customerId: { type: "string" }, note: { type: "string" } }, native, {
    naming: { toColumn: field => field === "customerId" ? "customer_id" : field, toField: field => field === "customer_id" ? "customerId" : field },
  });
  const customers = new Collection<{ name: string }>("customers", { name: { type: "string" } }, native);
  return { native, calls, o: orders.as("o"), c: customers.as("c") };
}

test("joined queries retain names, native values, and optional row types", async () => {
  const { native, calls, o, c } = fixture();
  const query = readFrom(native, o).leftJoin(c, eq(o.columns.note, c.columns.name))
    .select({ order: o.row(), customer: c.optionalRow() });
  const result = await query.where(eq(o.columns.note, "paid' OR TRUE")).limit(10).all();
  assert.equal(result.error, null);
  assert.deepEqual(result.data, [{ order: { customerId: "c", note: "paid" }, customer: null }]);
  const typed: Array<{ readonly order: Row<{ customerId: string; note: string }>; readonly customer: Row<{ name: string }> | null }> | null = result.data;
  assert.ok(typed);
  assert.deepEqual(calls[0].where, { op: "eq", left: { source:"o", field:"note" }, right: { value:"paid' OR TRUE" } });
  assert.equal((calls[0].from as { collection: string }).collection, "orders");
  await query.all();
  assert.equal(calls[1].where, undefined, "builders do not mutate earlier queries");
  if (false) {
    // @ts-expect-error left-joined row projections must be optional
    readFrom(native, o).leftJoin(c, eq(o.columns.note, c.columns.name)).select({ customer: c.row() });
    // @ts-expect-error incompatible literal
    eq(o.columns.note, 42);
  }
});

test("transaction queries reject escaped execution and normal reads return errors", async () => {
  const { native, calls, o } = fixture();
  let active = true;
  const escaped = readFrom(native, o, true, () => active).select({ order:o.row() });
  active = false;
  await assert.rejects(escaped.all(), { code:"TRANSACTION_SCOPE_EXPIRED" });
  assert.equal(calls.length, 0);
  const failed = await readFrom(native, o).all();
  assert.ok(failed.error);
  assert.equal(failed.data, null);
});

test("grouped reads use structured expressions and refuse another database", async () => {
  const { native, calls, o, c } = fixture();
  const other = fixture();
  assert.throws(() => readFrom(native, o).innerJoin(other.c, eq(o.columns.note, other.c.columns.name)), /same database/);
  await readFrom(native, o).innerJoin(c, eq(o.columns.note, c.columns.name))
    .select({ matches: count(c.columns.name, true) }).having(gt(count(), 0)).all();
  assert.deepEqual({ ...(calls[0].select as object) }, { matches:{ aggregate:"count", column:{source:"c", field:"name"}, distinct:true } });
});
