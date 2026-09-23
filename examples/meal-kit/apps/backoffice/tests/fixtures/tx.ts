// An in-memory stand-in for the transaction handle `packages/shared/src/server`
// takes, so those modules can be exercised without a database.
//
// It reproduces the behaviours those modules depend on and nothing else: a row
// carries `version`; a read hands back a detached row, so a write that lands
// afterwards is invisible to the value already in hand; `update` matches on
// every key of its filter and answers `null` when none matches, which is how
// optimistic concurrency reaches the caller; a successful `update` raises
// `version`; `find` understands the `$in` and `$lte` operators the stores use.
// The browser suite is what verifies the real engine agrees - this fixture
// verifies the modules' own arithmetic, ordering and refusals.
//
// `memoryTx` hands back the two halves separately: `tx` is what a subject
// receives, typed as the `Tx` it declares, and `table` is how a test seeds and
// inspects rows. Keeping them apart is what lets every call site stay honestly
// typed - the single conversion lives here, where the substitution is stated.

import type { Tx } from "@gather/meal-kit/server/core";

export type MemoryRow = Record<string, unknown> & { id: string; version: number };
type Row = MemoryRow;
type Filter = Record<string, unknown>;

function matches(row: Row, filter: Filter) {
  return Object.entries(filter).every(([key, want]) => {
    const has = row[key];
    if (want && typeof want === "object" && !Array.isArray(want)) {
      const ops = want as Record<string, unknown>;
      if ("$in" in ops) return (ops.$in as unknown[]).includes(has);
      if ("$lte" in ops) return String(has) <= String(ops.$lte);
      throw new Error(`Unsupported filter operator: ${Object.keys(ops)}`);
    }
    return has === want;
  });
}

export class Table {
  readonly rows: Row[] = [];
  #next = 0;
  constructor(private readonly name: string) {}

  /** Put a row in place without going through the subject under test. */
  seed(values: Record<string, unknown>) {
    const row = {
      version: 1,
      ...values,
      id: (values.id as string) ?? `${this.name}_${++this.#next}`,
    } as Row;
    this.rows.push(row);
    return row;
  }
  async insert(values: Record<string, unknown>) {
    return this.seed(values);
  }
  async get(selector: string | Filter) {
    const filter = typeof selector === "string" ? { id: selector } : selector;
    const row = this.rows.find((candidate) => matches(candidate, filter));
    return row ? ({ ...row } as Row) : null;
  }
  async update(filter: Filter, values: Record<string, unknown>) {
    const row = this.rows.find((candidate) => matches(candidate, filter));
    if (!row) return null;
    Object.assign(row, values, { version: row.version + 1 });
    return { ...row } as Row;
  }
  find(filter: Filter = {}) {
    let found = this.rows
      .filter((row) => matches(row, filter))
      .map((row) => ({ ...row }) as Row);
    const chain = {
      sort(order: Record<string, number>) {
        const [[key, direction]] = Object.entries(order);
        found = [...found].sort(
          (a, b) => (Number(a[key]) - Number(b[key])) * (direction < 0 ? -1 : 1),
        );
        return chain;
      },
      limit(count: number) {
        found = found.slice(0, count);
        return chain;
      },
      then: (resolve: (rows: Row[]) => unknown) =>
        Promise.resolve(found).then(resolve),
    };
    return chain;
  }
}

export function memoryTx() {
  const tables = new Map<string, Table>();
  const table = (name: string) => {
    if (!tables.has(name)) tables.set(name, new Table(name));
    return tables.get(name)!;
  };
  // The substitution, stated once: the subject is handed a `Tx`, and every
  // collection it names appears on first access.
  const tx = new Proxy({}, { get: (_, name: string) => table(name) }) as Tx;
  return { tx, table };
}

/** The `{ code, status }` a `fail` from `@gather/meal-kit/domain` carries. */
export async function refusal(run: () => unknown | Promise<unknown>) {
  try {
    await run();
  } catch (error) {
    const { code, status, message } = error as {
      code?: string;
      status?: number;
      message: string;
    };
    return { code, status, message };
  }
  throw new Error("Expected a refusal, but the call returned.");
}
