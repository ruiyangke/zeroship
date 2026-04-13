# @appbase/db SDK Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `@appbase/db` npm SDK — a Mongoose-compatible JS/TS package that wraps `appbase.db.*` native primitives with model definitions, validation, query chaining, and error mapping.

**Architecture:** Pure TypeScript, no external dependencies. Runs inside V8 isolate, bundled by esbuild into .appbundle. Calls `globalThis.appbase.db.*` native methods. Two schema styles (Mongoose object + `t` builder) normalize to a shared internal format.

**Tech Stack:** TypeScript, no runtime deps, node:test for unit tests

---

## File Map

| File | Responsibility |
|---|---|
| `sdks/db/package.json` | Package metadata, name `@appbase/db` |
| `sdks/db/tsconfig.json` | TS config |
| `sdks/db/src/index.ts` | Public exports: `model`, `t` |
| `sdks/db/src/types.ts` | `t` builder: `t.string()`, `t.number()`, etc. |
| `sdks/db/src/schema.ts` | `normalizeSchema()` — detect Mongoose vs builder, normalize |
| `sdks/db/src/validate.ts` | `validateDoc()`, `validatePartial()` — check against schema |
| `sdks/db/src/errors.ts` | `ValidationError`, `mapNativeError()` |
| `sdks/db/src/utils.ts` | Field mapping (`_id`↔`id`, `created_at`↔`createdAt`), aggregate translation |
| `sdks/db/src/query.ts` | `Query` thenable class with `sort/limit/skip/select` |
| `sdks/db/src/collection.ts` | `Collection` class — all CRUD methods |
| `sdks/db/src/model.ts` | `model(name, schema)` → Collection factory |
| `sdks/db/tests/types.test.ts` | Tests for `t` builder |
| `sdks/db/tests/schema.test.ts` | Tests for both schema styles |
| `sdks/db/tests/validate.test.ts` | Tests for validation rules |
| `sdks/db/tests/utils.test.ts` | Tests for field mapping + aggregate translation |
| `sdks/db/tests/query.test.ts` | Tests for Query chain |
| `sdks/db/tests/collection.test.ts` | Tests for Collection methods (mocked native) |

---

### Task 1: Project scaffold + types.ts

**Files:**
- Create: `sdks/db/package.json`
- Create: `sdks/db/tsconfig.json`
- Create: `sdks/db/src/types.ts`
- Create: `sdks/db/src/index.ts`
- Create: `sdks/db/tests/types.test.ts`

- [ ] **Step 1: Create package.json**

```json
{
  "name": "@appbase/db",
  "version": "0.1.0",
  "description": "Mongoose-compatible database SDK for appbase",
  "main": "src/index.ts",
  "types": "src/index.ts",
  "scripts": {
    "test": "node --import tsx --test tests/*.test.ts"
  },
  "devDependencies": {
    "tsx": "^4.0.0",
    "typescript": "^5.0.0"
  }
}
```

- [ ] **Step 2: Create tsconfig.json**

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ES2022",
    "moduleResolution": "bundler",
    "strict": true,
    "esModuleInterop": true,
    "declaration": true,
    "outDir": "dist",
    "rootDir": "src"
  },
  "include": ["src"]
}
```

- [ ] **Step 3: Write types.test.ts**

```typescript
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/types.js";

describe("t builder", () => {
  it("t.string() creates string type", () => {
    const def = t.string()._def;
    assert.equal(def.type, "string");
  });

  it("t.number() creates number type", () => {
    const def = t.number()._def;
    assert.equal(def.type, "number");
  });

  it("t.boolean() creates boolean type", () => {
    const def = t.boolean()._def;
    assert.equal(def.type, "boolean");
  });

  it("t.date() creates date type", () => {
    const def = t.date()._def;
    assert.equal(def.type, "date");
  });

  it("t.json() creates json type", () => {
    const def = t.json()._def;
    assert.equal(def.type, "json");
  });

  it("t.array(t.string()) creates array type", () => {
    const def = t.array(t.string())._def;
    assert.equal(def.type, "array");
    assert.equal(def.items, "string");
  });

  it("chaining modifiers", () => {
    const def = t.string().required().unique().min(1).max(100).default("hello")._def;
    assert.equal(def.type, "string");
    assert.equal(def.required, true);
    assert.equal(def.unique, true);
    assert.equal(def.min, 1);
    assert.equal(def.max, 100);
    assert.equal(def.default, "hello");
  });

  it("enum modifier", () => {
    const def = t.string().enum("a", "b", "c")._def;
    assert.deepEqual(def.enum, ["a", "b", "c"]);
  });

  it("pattern modifier", () => {
    const def = t.string().pattern(/^[a-z]+$/)._def;
    assert.ok(def.pattern instanceof RegExp);
  });
});
```

- [ ] **Step 4: Implement types.ts**

```typescript
export interface FieldDef {
  type: string;
  required?: boolean;
  unique?: boolean;
  index?: boolean;
  default?: unknown;
  min?: number;
  max?: number;
  enum?: string[];
  pattern?: RegExp;
  items?: string;
}

export class TypeBuilder {
  _def: FieldDef;

  constructor(type: string) {
    this._def = { type };
  }

  required(): this { this._def.required = true; return this; }
  unique(): this { this._def.unique = true; return this; }
  index(): this { this._def.index = true; return this; }
  default(val: unknown): this { this._def.default = val; return this; }
  min(n: number): this { this._def.min = n; return this; }
  max(n: number): this { this._def.max = n; return this; }
  enum(...values: string[]): this { this._def.enum = values; return this; }
  pattern(re: RegExp): this { this._def.pattern = re; return this; }
}

export const t = {
  string: () => new TypeBuilder("string"),
  number: () => new TypeBuilder("number"),
  boolean: () => new TypeBuilder("boolean"),
  date: () => new TypeBuilder("date"),
  json: () => new TypeBuilder("json"),
  array: (itemType: TypeBuilder) => {
    const b = new TypeBuilder("array");
    b._def.items = itemType._def.type;
    return b;
  },
};
```

- [ ] **Step 5: Create initial index.ts**

```typescript
export { t, TypeBuilder } from "./types.js";
export type { FieldDef } from "./types.js";
```

- [ ] **Step 6: Install deps and run tests**

```bash
cd sdks/db && npm install && npm test
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): scaffold package + t type builder"
```

---

### Task 2: Schema normalization

**Files:**
- Create: `sdks/db/src/schema.ts`
- Create: `sdks/db/tests/schema.test.ts`

- [ ] **Step 1: Write schema.test.ts**

```typescript
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { normalizeSchema } from "../src/schema.js";
import { t } from "../src/types.js";

describe("normalizeSchema", () => {
  it("normalizes Mongoose style", () => {
    const schema = normalizeSchema({
      name: { type: String, required: true },
      age: { type: Number, min: 0 },
      active: { type: Boolean, default: true },
      bio: { type: String },
      tags: { type: [String] },
      settings: { type: Object },
      birthday: { type: Date },
    });
    assert.equal(schema.name.type, "string");
    assert.equal(schema.name.required, true);
    assert.equal(schema.age.type, "number");
    assert.equal(schema.age.min, 0);
    assert.equal(schema.active.type, "boolean");
    assert.equal(schema.active.default, true);
    assert.equal(schema.tags.type, "array");
    assert.equal(schema.tags.items, "string");
    assert.equal(schema.settings.type, "json");
    assert.equal(schema.birthday.type, "date");
  });

  it("normalizes builder style", () => {
    const schema = normalizeSchema({
      name: t.string().required(),
      age: t.number().min(0),
    });
    assert.equal(schema.name.type, "string");
    assert.equal(schema.name.required, true);
    assert.equal(schema.age.type, "number");
    assert.equal(schema.age.min, 0);
  });

  it("supports mixed styles", () => {
    const schema = normalizeSchema({
      name: { type: String, required: true },
      age: t.number().min(0),
    });
    assert.equal(schema.name.type, "string");
    assert.equal(schema.age.type, "number");
  });

  it("Mongoose enum and default", () => {
    const schema = normalizeSchema({
      role: { type: String, enum: ["user", "admin"], default: "user" },
    });
    assert.deepEqual(schema.role.enum, ["user", "admin"]);
    assert.equal(schema.role.default, "user");
  });

  it("Mongoose unique", () => {
    const schema = normalizeSchema({
      email: { type: String, required: true, unique: true },
    });
    assert.equal(schema.email.unique, true);
    assert.equal(schema.email.required, true);
  });
});
```

- [ ] **Step 2: Implement schema.ts**

```typescript
import { TypeBuilder, type FieldDef } from "./types.js";

export type NormalizedSchema = Record<string, FieldDef>;

const TYPE_MAP = new Map<unknown, string>([
  [String, "string"],
  [Number, "number"],
  [Boolean, "boolean"],
  [Date, "date"],
  [Object, "json"],
]);

function normalizeMongooseField(def: Record<string, unknown>): FieldDef {
  const rawType = def.type;
  let type: string;
  let items: string | undefined;

  if (Array.isArray(rawType)) {
    type = "array";
    items = TYPE_MAP.get(rawType[0]) ?? "string";
  } else {
    type = TYPE_MAP.get(rawType) ?? "string";
  }

  const field: FieldDef = { type };
  if (items) field.items = items;
  if (def.required) field.required = true;
  if (def.unique) field.unique = true;
  if (def.index) field.index = true;
  if (def.default !== undefined) field.default = def.default;
  if (def.min !== undefined) field.min = def.min as number;
  if (def.max !== undefined) field.max = def.max as number;
  if (def.enum) field.enum = def.enum as string[];
  if (def.pattern) field.pattern = def.pattern as RegExp;

  return field;
}

export function normalizeSchema(raw: Record<string, unknown>): NormalizedSchema {
  const schema: NormalizedSchema = {};
  for (const [key, value] of Object.entries(raw)) {
    if (value instanceof TypeBuilder) {
      schema[key] = value._def;
    } else if (typeof value === "object" && value !== null && "type" in value) {
      schema[key] = normalizeMongooseField(value as Record<string, unknown>);
    } else {
      throw new Error(`Invalid schema field "${key}": must be a type builder or Mongoose-style object`);
    }
  }
  return schema;
}
```

- [ ] **Step 3: Run tests**

```bash
cd sdks/db && npm test
```

- [ ] **Step 4: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): schema normalization for Mongoose + builder styles"
```

---

### Task 3: Validation

**Files:**
- Create: `sdks/db/src/validate.ts`
- Create: `sdks/db/src/errors.ts`
- Create: `sdks/db/tests/validate.test.ts`

- [ ] **Step 1: Write errors.ts**

```typescript
export class ValidationError extends Error {
  name = "ValidationError";
  errors: Record<string, { message: string; path: string }>;

  constructor(errors: Record<string, { message: string; path: string }>) {
    const paths = Object.keys(errors).join(", ");
    super(`Validation failed: ${paths}`);
    this.errors = errors;
  }
}

export function mapNativeError(msg: string): Error {
  if (msg.includes("unique") || msg.includes("duplicate")) {
    const err = new Error(`duplicate key error: ${msg}`);
    (err as any).code = 11000;
    return err;
  }
  return new Error(msg);
}
```

- [ ] **Step 2: Write validate.test.ts**

```typescript
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { validateDoc, validatePartial } from "../src/validate.js";
import { normalizeSchema } from "../src/schema.js";
import { ValidationError } from "../src/errors.js";

const schema = normalizeSchema({
  name: { type: String, required: true, min: 1, max: 50 },
  email: { type: String, required: true },
  age: { type: Number, min: 0, max: 150 },
  role: { type: String, enum: ["user", "admin"], default: "user" },
  active: { type: Boolean },
});

describe("validateDoc (insert)", () => {
  it("passes valid doc", () => {
    const doc = validateDoc({ name: "Alice", email: "a@b.com" }, schema);
    assert.equal(doc.name, "Alice");
    assert.equal(doc.role, "user"); // default applied
  });

  it("throws on missing required field", () => {
    assert.throws(
      () => validateDoc({ name: "Alice" }, schema),
      (e: any) => e instanceof ValidationError && "email" in e.errors
    );
  });

  it("throws on wrong type", () => {
    assert.throws(
      () => validateDoc({ name: 123, email: "a@b.com" }, schema),
      (e: any) => e instanceof ValidationError && "name" in e.errors
    );
  });

  it("throws on min length violation", () => {
    assert.throws(
      () => validateDoc({ name: "", email: "a@b.com" }, schema),
      (e: any) => e instanceof ValidationError && "name" in e.errors
    );
  });

  it("throws on max length violation", () => {
    assert.throws(
      () => validateDoc({ name: "x".repeat(51), email: "a@b.com" }, schema),
      (e: any) => e instanceof ValidationError && "name" in e.errors
    );
  });

  it("throws on number min violation", () => {
    assert.throws(
      () => validateDoc({ name: "Alice", email: "a@b.com", age: -1 }, schema),
      (e: any) => e instanceof ValidationError && "age" in e.errors
    );
  });

  it("throws on enum violation", () => {
    assert.throws(
      () => validateDoc({ name: "Alice", email: "a@b.com", role: "superadmin" }, schema),
      (e: any) => e instanceof ValidationError && "role" in e.errors
    );
  });

  it("applies defaults", () => {
    const doc = validateDoc({ name: "Alice", email: "a@b.com" }, schema);
    assert.equal(doc.role, "user");
  });

  it("allows optional fields to be absent", () => {
    const doc = validateDoc({ name: "Alice", email: "a@b.com" }, schema);
    assert.equal(doc.age, undefined);
    assert.equal(doc.active, undefined);
  });
});

describe("validatePartial (update)", () => {
  it("validates only provided fields", () => {
    const doc = validatePartial({ name: "Bob" }, schema);
    assert.equal(doc.name, "Bob");
  });

  it("throws on invalid provided field", () => {
    assert.throws(
      () => validatePartial({ age: -1 }, schema),
      (e: any) => e instanceof ValidationError
    );
  });

  it("does not require required fields", () => {
    const doc = validatePartial({ age: 25 }, schema);
    assert.equal(doc.age, 25);
  });
});
```

- [ ] **Step 3: Implement validate.ts**

```typescript
import type { FieldDef } from "./types.js";
import type { NormalizedSchema } from "./schema.js";
import { ValidationError } from "./errors.js";

type Errors = Record<string, { message: string; path: string }>;

function validateField(key: string, value: unknown, def: FieldDef, errors: Errors): void {
  if (value === undefined || value === null) {
    return; // absence is handled separately by required check
  }

  // Type check
  const typeChecks: Record<string, (v: unknown) => boolean> = {
    string: (v) => typeof v === "string",
    number: (v) => typeof v === "number",
    boolean: (v) => typeof v === "boolean",
    date: (v) => typeof v === "number" || v instanceof Date,
    json: (v) => typeof v === "object",
    array: (v) => Array.isArray(v),
  };

  const check = typeChecks[def.type];
  if (check && !check(value)) {
    errors[key] = { message: `expected ${def.type}, got ${typeof value}`, path: key };
    return;
  }

  // Min/max for strings
  if (def.type === "string" && typeof value === "string") {
    if (def.min !== undefined && value.length < def.min) {
      errors[key] = { message: `must be at least ${def.min} characters`, path: key };
      return;
    }
    if (def.max !== undefined && value.length > def.max) {
      errors[key] = { message: `must be at most ${def.max} characters`, path: key };
      return;
    }
  }

  // Min/max for numbers
  if (def.type === "number" && typeof value === "number") {
    if (def.min !== undefined && value < def.min) {
      errors[key] = { message: `must be >= ${def.min}`, path: key };
      return;
    }
    if (def.max !== undefined && value > def.max) {
      errors[key] = { message: `must be <= ${def.max}`, path: key };
      return;
    }
  }

  // Enum
  if (def.enum && !def.enum.includes(value as string)) {
    errors[key] = { message: `must be one of: ${def.enum.join(", ")}`, path: key };
    return;
  }

  // Pattern
  if (def.pattern && typeof value === "string" && !def.pattern.test(value)) {
    errors[key] = { message: `must match pattern ${def.pattern}`, path: key };
  }
}

export function validateDoc(
  doc: Record<string, unknown>,
  schema: NormalizedSchema
): Record<string, unknown> {
  const errors: Errors = {};
  const result = { ...doc };

  for (const [key, def] of Object.entries(schema)) {
    // Apply defaults
    if ((result[key] === undefined || result[key] === null) && def.default !== undefined) {
      result[key] = typeof def.default === "function" ? def.default() : def.default;
    }

    // Required check
    if (def.required && (result[key] === undefined || result[key] === null)) {
      errors[key] = { message: "required", path: key };
      continue;
    }

    // Field validation
    if (result[key] !== undefined && result[key] !== null) {
      validateField(key, result[key], def, errors);
    }
  }

  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }

  return result;
}

export function validatePartial(
  doc: Record<string, unknown>,
  schema: NormalizedSchema
): Record<string, unknown> {
  const errors: Errors = {};

  for (const [key, value] of Object.entries(doc)) {
    const def = schema[key];
    if (!def) continue; // unknown fields pass through
    if (value !== undefined && value !== null) {
      validateField(key, value, def, errors);
    }
  }

  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }

  return doc;
}
```

- [ ] **Step 4: Run tests**

```bash
cd sdks/db && npm test
```

- [ ] **Step 5: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): validation engine + ValidationError"
```

---

### Task 4: Utils (field mapping + aggregate translation)

**Files:**
- Create: `sdks/db/src/utils.ts`
- Create: `sdks/db/tests/utils.test.ts`

- [ ] **Step 1: Write utils.test.ts**

```typescript
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  mapFieldsOutbound,
  mapFieldsInbound,
  mapResultDoc,
  translateAggregatePipeline,
} from "../src/utils.js";

describe("field mapping", () => {
  it("outbound: _id → id in filter", () => {
    const f = mapFieldsOutbound({ _id: "abc", name: "Alice" });
    assert.equal(f.id, "abc");
    assert.equal(f.name, "Alice");
    assert.equal(f._id, undefined);
  });

  it("inbound: id → _id, created_at → createdAt", () => {
    const doc = mapResultDoc({
      id: "abc",
      name: "Alice",
      created_at: 123,
      updated_at: 456,
    });
    assert.equal(doc._id, "abc");
    assert.equal(doc.name, "Alice");
    assert.equal(doc.createdAt, 123);
    assert.equal(doc.updatedAt, 456);
    assert.equal(doc.id, undefined);
    assert.equal(doc.created_at, undefined);
  });

  it("inbound: handles missing auto fields", () => {
    const doc = mapResultDoc({ name: "Bob" });
    assert.equal(doc.name, "Bob");
    assert.equal(doc._id, undefined);
  });
});

describe("aggregate translation", () => {
  it("translates _id to by and strips $ from field refs", () => {
    const pipeline = [
      { $match: { status: "active" } },
      { $group: { _id: "$category", count: { $sum: 1 } } },
      { $sort: { count: -1 } },
    ];
    const translated = translateAggregatePipeline(pipeline);
    assert.deepEqual(translated[0], { $match: { status: "active" } });
    assert.equal(translated[1].$group.by, "category");
    assert.equal(translated[1].$group._id, undefined);
    assert.deepEqual(translated[1].$group.count, { $count: true });
  });

  it("translates $sum: 1 to $count: true", () => {
    const pipeline = [
      { $group: { _id: "$region", n: { $sum: 1 } } },
    ];
    const translated = translateAggregatePipeline(pipeline);
    assert.deepEqual(translated[0].$group.n, { $count: true });
  });

  it("translates $sum/$avg/$min/$max with $ field refs", () => {
    const pipeline = [
      { $group: { _id: "$cat", total: { $sum: "$price" }, avg: { $avg: "$price" } } },
    ];
    const translated = translateAggregatePipeline(pipeline);
    assert.deepEqual(translated[0].$group.total, { $sum: "price" });
    assert.deepEqual(translated[0].$group.avg, { $avg: "price" });
  });

  it("handles multi-field group: _id: { a: '$x', b: '$y' }", () => {
    const pipeline = [
      { $group: { _id: { region: "$region", cat: "$category" }, n: { $sum: 1 } } },
    ];
    const translated = translateAggregatePipeline(pipeline);
    assert.deepEqual(translated[0].$group.by, ["region", "category"]);
  });
});
```

- [ ] **Step 2: Implement utils.ts**

```typescript
// --- Field mapping (auto-generated fields only) ---

const OUTBOUND_MAP: Record<string, string> = { _id: "id" };
const INBOUND_MAP: Record<string, string> = {
  id: "_id",
  created_at: "createdAt",
  updated_at: "updatedAt",
};

export function mapFieldsOutbound(obj: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj)) {
    result[OUTBOUND_MAP[key] ?? key] = value;
  }
  return result;
}

export function mapResultDoc(doc: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(doc)) {
    result[INBOUND_MAP[key] ?? key] = value;
  }
  return result;
}

// --- Deep filter mapping (_id → id in nested $and/$or) ---

export function mapFilterOutbound(filter: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(filter)) {
    const mappedKey = OUTBOUND_MAP[key] ?? key;
    if ((key === "$and" || key === "$or") && Array.isArray(value)) {
      result[key] = value.map((v: any) => mapFilterOutbound(v));
    } else if (key === "$not" && typeof value === "object" && value !== null) {
      result[key] = mapFilterOutbound(value as Record<string, unknown>);
    } else {
      result[mappedKey] = value;
    }
  }
  return result;
}

// --- Aggregate pipeline translation ---

function stripDollar(s: string): string {
  return s.startsWith("$") ? s.slice(1) : s;
}

function translateGroupStage(group: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  const idVal = group._id;

  // _id → by
  if (typeof idVal === "string") {
    result.by = stripDollar(idVal);
  } else if (typeof idVal === "object" && idVal !== null && !Array.isArray(idVal)) {
    // { region: "$region", cat: "$category" } → ["region", "category"]
    result.by = Object.values(idVal as Record<string, string>).map(stripDollar);
  }

  // Translate aggregation operators
  for (const [key, value] of Object.entries(group)) {
    if (key === "_id") continue;
    if (typeof value === "object" && value !== null) {
      const agg = value as Record<string, unknown>;
      const op = Object.keys(agg)[0];
      const val = agg[op];
      if (op === "$sum" && val === 1) {
        result[key] = { $count: true };
      } else if (typeof val === "string") {
        result[key] = { [op]: stripDollar(val as string) };
      } else {
        result[key] = value;
      }
    } else {
      result[key] = value;
    }
  }

  return result;
}

export function translateAggregatePipeline(
  pipeline: Record<string, unknown>[]
): Record<string, unknown>[] {
  return pipeline.map((stage) => {
    if ("$group" in stage) {
      return { $group: translateGroupStage(stage.$group as Record<string, unknown>) };
    }
    // $match, $sort, $limit pass through
    return stage;
  });
}
```

- [ ] **Step 3: Run tests**

```bash
cd sdks/db && npm test
```

- [ ] **Step 4: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): field mapping utils + aggregate pipeline translation"
```

---

### Task 5: Query class (thenable)

**Files:**
- Create: `sdks/db/src/query.ts`
- Create: `sdks/db/tests/query.test.ts`

- [ ] **Step 1: Write query.test.ts**

```typescript
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { Query } from "../src/query.js";

// Mock native
const calls: any[] = [];
const mockNative = {
  find: async (_col: string, _filter: any, _opts: any) => {
    calls.push({ method: "find", args: [_col, _filter, _opts] });
    return "[]";
  },
};

describe("Query", () => {
  it("collects sort/limit/skip/select", () => {
    const q = new Query("users", { role: "admin" }, mockNative as any);
    q.sort({ name: 1 }).limit(10).skip(20).select("name email");
    assert.deepEqual(q._sort, { name: 1 });
    assert.equal(q._limit, 10);
    assert.equal(q._skip, 20);
    assert.deepEqual(q._select, ["name", "email"]);
  });

  it("select accepts array", () => {
    const q = new Query("users", {}, mockNative as any);
    q.select(["name", "email"]);
    assert.deepEqual(q._select, ["name", "email"]);
  });

  it("is thenable — await triggers exec", async () => {
    calls.length = 0;
    const q = new Query("users", { role: "admin" }, mockNative as any);
    q.sort({ name: 1 }).limit(5);
    await q;
    assert.equal(calls.length, 1);
    assert.equal(calls[0].args[0], "users");
    assert.deepEqual(calls[0].args[2], {
      orderBy: { name: 1 },
      limit: 5,
    });
  });
});
```

- [ ] **Step 2: Implement query.ts**

```typescript
import { mapFilterOutbound, mapResultDoc } from "./utils.js";

interface NativeDb {
  find(collection: string, filter: unknown, opts: unknown): Promise<string>;
}

export class Query {
  _collection: string;
  _filter: Record<string, unknown>;
  _native: NativeDb;
  _sort: Record<string, number> | null = null;
  _limit: number | null = null;
  _skip: number | null = null;
  _select: string[] | null = null;

  constructor(collection: string, filter: Record<string, unknown>, native: NativeDb) {
    this._collection = collection;
    this._filter = filter;
    this._native = native;
  }

  sort(s: Record<string, number>): this { this._sort = s; return this; }
  limit(n: number): this { this._limit = n; return this; }
  skip(n: number): this { this._skip = n; return this; }

  select(fields: string | string[]): this {
    if (typeof fields === "string") {
      this._select = fields.split(/\s+/).filter(Boolean);
    } else {
      this._select = fields;
    }
    return this;
  }

  then(
    resolve?: (value: Record<string, unknown>[]) => unknown,
    reject?: (reason: unknown) => unknown
  ): Promise<unknown> {
    return this._exec().then(resolve, reject);
  }

  async _exec(): Promise<Record<string, unknown>[]> {
    const opts: Record<string, unknown> = {};
    if (this._sort) opts.orderBy = this._sort;
    if (this._limit) opts.limit = this._limit;
    if (this._skip) opts.offset = this._skip;
    if (this._select) opts.select = this._select;

    const filter = mapFilterOutbound(this._filter);
    const raw = await this._native.find(this._collection, filter, opts);
    const rows: Record<string, unknown>[] = typeof raw === "string" ? JSON.parse(raw) : raw;
    return rows.map(mapResultDoc);
  }
}
```

- [ ] **Step 3: Run tests**

```bash
cd sdks/db && npm test
```

- [ ] **Step 4: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): Query thenable class with sort/limit/skip/select"
```

---

### Task 6: Collection class

**Files:**
- Create: `sdks/db/src/collection.ts`
- Create: `sdks/db/tests/collection.test.ts`

- [ ] **Step 1: Write collection.test.ts**

```typescript
import { describe, it, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../src/collection.js";
import { normalizeSchema } from "../src/schema.js";
import { ValidationError } from "../src/errors.js";

// Track native calls
let calls: { method: string; args: any[] }[] = [];

const mockNative = {
  insert: async (col: string, doc: any) => {
    calls.push({ method: "insert", args: [col, doc] });
    return JSON.stringify({ id: 1, ...doc, created_at: 1000, updated_at: 1000 });
  },
  insertMany: async (col: string, docs: any) => {
    calls.push({ method: "insertMany", args: [col, docs] });
    return JSON.stringify(docs.map((d: any, i: number) => ({ id: i + 1, ...d })));
  },
  findOne: async (col: string, filter: any) => {
    calls.push({ method: "findOne", args: [col, filter] });
    return JSON.stringify({ id: 1, name: "Alice", created_at: 1000 });
  },
  find: async (col: string, filter: any, opts: any) => {
    calls.push({ method: "find", args: [col, filter, opts] });
    return JSON.stringify([{ id: 1, name: "Alice" }]);
  },
  updateOne: async (col: string, filter: any, update: any) => {
    calls.push({ method: "updateOne", args: [col, filter, update] });
    return JSON.stringify({ id: 1, name: "Updated" });
  },
  updateMany: async (col: string, filter: any, update: any) => {
    calls.push({ method: "updateMany", args: [col, filter, update] });
    return JSON.stringify({ updated: 3 });
  },
  deleteOne: async (col: string, filter: any) => {
    calls.push({ method: "deleteOne", args: [col, filter] });
    return JSON.stringify({ id: 1, name: "Deleted" });
  },
  deleteMany: async (col: string, filter: any) => {
    calls.push({ method: "deleteMany", args: [col, filter] });
    return JSON.stringify({ deleted: 5 });
  },
  count: async (col: string, filter: any) => {
    calls.push({ method: "count", args: [col, filter] });
    return JSON.stringify({ count: 42 });
  },
  distinct: async (col: string, field: string, filter: any) => {
    calls.push({ method: "distinct", args: [col, field, filter] });
    return JSON.stringify(["admin", "user"]);
  },
  aggregate: async (col: string, pipeline: any) => {
    calls.push({ method: "aggregate", args: [col, pipeline] });
    return JSON.stringify([{ category: "tech", count: 5 }]);
  },
};

const schema = normalizeSchema({
  name: { type: String, required: true },
  email: { type: String, required: true },
  role: { type: String, default: "user" },
});

describe("Collection", () => {
  beforeEach(() => { calls = []; });

  it("create() validates and calls insert", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.create({ name: "Alice", email: "a@b.com" });
    assert.equal(calls[0].method, "insert");
    assert.equal(calls[0].args[0], "users");
    assert.equal(result._id, 1);
    assert.equal(result.createdAt, 1000);
  });

  it("create() applies defaults", async () => {
    const col = new Collection("users", schema, mockNative as any);
    await col.create({ name: "Alice", email: "a@b.com" });
    assert.equal(calls[0].args[1].role, "user");
  });

  it("create() throws ValidationError on missing required", async () => {
    const col = new Collection("users", schema, mockNative as any);
    await assert.rejects(
      () => col.create({ name: "Alice" }),
      (e: any) => e instanceof ValidationError
    );
  });

  it("findOne() maps _id in filter", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.findOne({ _id: "abc" });
    assert.equal(calls[0].args[1].id, "abc");
    assert.equal(result!._id, 1);
  });

  it("find() returns Query", () => {
    const col = new Collection("users", schema, mockNative as any);
    const q = col.find({ role: "admin" });
    assert.ok(typeof q.sort === "function");
    assert.ok(typeof q.limit === "function");
    assert.ok(typeof q.then === "function");
  });

  it("updateOne() maps _id and returns matchedCount/modifiedCount", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.updateOne({ _id: "abc" }, { $set: { name: "Bob" } });
    assert.equal(calls[0].args[1].id, "abc");
    assert.equal(result.modifiedCount, 1);
  });

  it("updateMany() returns matchedCount/modifiedCount", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.updateMany({ role: "user" }, { $set: { role: "member" } });
    assert.equal(result.modifiedCount, 3);
  });

  it("deleteOne() returns deletedCount", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.deleteOne({ _id: "abc" });
    assert.equal(result.deletedCount, 1);
  });

  it("deleteMany() returns deletedCount", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const result = await col.deleteMany({ role: "old" });
    assert.equal(result.deletedCount, 5);
  });

  it("countDocuments()", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const count = await col.countDocuments({ role: "admin" });
    assert.equal(count, 42);
  });

  it("distinct()", async () => {
    const col = new Collection("users", schema, mockNative as any);
    const values = await col.distinct("role");
    assert.deepEqual(values, ["admin", "user"]);
  });

  it("aggregate() translates pipeline", async () => {
    const col = new Collection("users", schema, mockNative as any);
    await col.aggregate([
      { $group: { _id: "$role", count: { $sum: 1 } } },
    ]);
    // Should have translated _id → by, $sum:1 → $count:true
    const pipeline = calls[0].args[1];
    assert.equal(pipeline[0].$group.by, "role");
    assert.deepEqual(pipeline[0].$group.count, { $count: true });
  });
});
```

- [ ] **Step 2: Implement collection.ts**

```typescript
import type { NormalizedSchema } from "./schema.js";
import { validateDoc, validatePartial } from "./validate.js";
import { mapFilterOutbound, mapResultDoc, translateAggregatePipeline } from "./utils.js";
import { mapNativeError } from "./errors.js";
import { Query } from "./query.js";

interface NativeDb {
  insert(collection: string, doc: unknown): Promise<string>;
  insertMany(collection: string, docs: unknown): Promise<string>;
  findOne(collection: string, filter: unknown): Promise<string | null>;
  find(collection: string, filter: unknown, opts: unknown): Promise<string>;
  updateOne(collection: string, filter: unknown, update: unknown): Promise<string>;
  updateMany(collection: string, filter: unknown, update: unknown): Promise<string>;
  deleteOne(collection: string, filter: unknown): Promise<string>;
  deleteMany(collection: string, filter: unknown): Promise<string>;
  count(collection: string, filter: unknown): Promise<string>;
  distinct(collection: string, field: string, filter: unknown): Promise<string>;
  aggregate(collection: string, pipeline: unknown): Promise<string>;
}

function parse(raw: string | null): unknown {
  if (raw === null || raw === "null") return null;
  return typeof raw === "string" ? JSON.parse(raw) : raw;
}

function extractUpdateFields(update: Record<string, unknown>): Record<string, unknown> {
  // Extract plain fields from $set or top-level non-operator keys
  const fields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(update)) {
    if (key === "$set" && typeof value === "object" && value !== null) {
      Object.assign(fields, value);
    } else if (!key.startsWith("$")) {
      fields[key] = value;
    }
  }
  return fields;
}

export class Collection {
  private _name: string;
  private _schema: NormalizedSchema;
  private _native: NativeDb;

  constructor(name: string, schema: NormalizedSchema, native: NativeDb) {
    this._name = name;
    this._schema = schema;
    this._native = native;
  }

  async create(doc: Record<string, unknown>): Promise<Record<string, unknown>> {
    const validated = validateDoc(doc, this._schema);
    try {
      const raw = await this._native.insert(this._name, validated);
      return mapResultDoc(parse(raw) as Record<string, unknown>);
    } catch (e: any) {
      throw mapNativeError(e.message ?? String(e));
    }
  }

  async insertMany(docs: Record<string, unknown>[]): Promise<Record<string, unknown>[]> {
    const validated = docs.map((d) => validateDoc(d, this._schema));
    try {
      const raw = await this._native.insertMany(this._name, validated);
      const rows = parse(raw) as Record<string, unknown>[];
      return rows.map(mapResultDoc);
    } catch (e: any) {
      throw mapNativeError(e.message ?? String(e));
    }
  }

  async findOne(filter: Record<string, unknown>): Promise<Record<string, unknown> | null> {
    const mapped = mapFilterOutbound(filter);
    const raw = await this._native.findOne(this._name, mapped);
    const doc = parse(raw);
    if (doc === null) return null;
    return mapResultDoc(doc as Record<string, unknown>);
  }

  find(filter: Record<string, unknown> = {}): Query {
    return new Query(this._name, filter, this._native);
  }

  async updateOne(
    filter: Record<string, unknown>,
    update: Record<string, unknown>
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const plainFields = extractUpdateFields(update);
    if (Object.keys(plainFields).length > 0) {
      validatePartial(plainFields, this._schema);
    }
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.updateOne(this._name, mapped, update);
      const result = parse(raw);
      const found = result !== null;
      return { matchedCount: found ? 1 : 0, modifiedCount: found ? 1 : 0 };
    } catch (e: any) {
      throw mapNativeError(e.message ?? String(e));
    }
  }

  async updateMany(
    filter: Record<string, unknown>,
    update: Record<string, unknown>
  ): Promise<{ matchedCount: number; modifiedCount: number }> {
    const plainFields = extractUpdateFields(update);
    if (Object.keys(plainFields).length > 0) {
      validatePartial(plainFields, this._schema);
    }
    const mapped = mapFilterOutbound(filter);
    try {
      const raw = await this._native.updateMany(this._name, mapped, update);
      const result = parse(raw) as Record<string, unknown>;
      const count = (result.updated as number) ?? 0;
      return { matchedCount: count, modifiedCount: count };
    } catch (e: any) {
      throw mapNativeError(e.message ?? String(e));
    }
  }

  async deleteOne(
    filter: Record<string, unknown>
  ): Promise<{ deletedCount: number }> {
    const mapped = mapFilterOutbound(filter);
    const raw = await this._native.deleteOne(this._name, mapped);
    const result = parse(raw);
    return { deletedCount: result !== null ? 1 : 0 };
  }

  async deleteMany(
    filter: Record<string, unknown>
  ): Promise<{ deletedCount: number }> {
    const mapped = mapFilterOutbound(filter);
    const raw = await this._native.deleteMany(this._name, mapped);
    const result = parse(raw) as Record<string, unknown>;
    return { deletedCount: (result.deleted as number) ?? 0 };
  }

  async countDocuments(filter: Record<string, unknown> = {}): Promise<number> {
    const mapped = mapFilterOutbound(filter);
    const raw = await this._native.count(this._name, mapped);
    const result = parse(raw) as Record<string, unknown>;
    return (result.count as number) ?? 0;
  }

  async distinct(field: string, filter: Record<string, unknown> = {}): Promise<unknown[]> {
    const mapped = mapFilterOutbound(filter);
    const raw = await this._native.distinct(this._name, field, mapped);
    return parse(raw) as unknown[];
  }

  async aggregate(pipeline: Record<string, unknown>[]): Promise<Record<string, unknown>[]> {
    const translated = translateAggregatePipeline(pipeline);
    const raw = await this._native.aggregate(this._name, translated);
    const rows = parse(raw) as Record<string, unknown>[];
    return rows.map(mapResultDoc);
  }
}
```

- [ ] **Step 3: Run tests**

```bash
cd sdks/db && npm test
```

- [ ] **Step 4: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): Collection class with all CRUD methods"
```

---

### Task 7: model() factory + index.ts + wiring to globalThis.appbase.db

**Files:**
- Create: `sdks/db/src/model.ts`
- Modify: `sdks/db/src/index.ts`

- [ ] **Step 1: Implement model.ts**

```typescript
import { normalizeSchema } from "./schema.js";
import { Collection } from "./collection.js";

// In the V8 runtime, appbase.db.* is on globalThis.appbase.db
// When running in Node.js tests, it won't exist — callers pass a mock.
function getNativeDb(): any {
  if (typeof globalThis !== "undefined" && (globalThis as any).appbase?.db) {
    return (globalThis as any).appbase.db;
  }
  throw new Error("@appbase/db: native appbase.db.* not available — are you running inside appbase?");
}

export function model(
  name: string,
  schema: Record<string, unknown>,
  nativeOverride?: any
): Collection {
  const normalized = normalizeSchema(schema);
  const native = nativeOverride ?? getNativeDb();
  return new Collection(name, normalized, native);
}
```

- [ ] **Step 2: Update index.ts with all exports**

```typescript
export { model } from "./model.js";
export { t, TypeBuilder } from "./types.js";
export type { FieldDef } from "./types.js";
export { Collection } from "./collection.js";
export { Query } from "./query.js";
export { normalizeSchema } from "./schema.js";
export type { NormalizedSchema } from "./schema.js";
export { ValidationError, mapNativeError } from "./errors.js";
export { validateDoc, validatePartial } from "./validate.js";
```

- [ ] **Step 3: Run all tests**

```bash
cd sdks/db && npm test
```

Expected: ALL pass.

- [ ] **Step 4: Commit**

```bash
git add sdks/db/
git commit -m "feat(sdk-db): model() factory + complete public API"
```

---

### Task 8: E2E integration test

**Files:**
- None created in sdks/db — this tests the full platform pipeline.

- [ ] **Step 1: Build the worker**

```bash
cargo build --release -p appbase-worker -p appbase-control -p appbase-gateway -p appbase
```

- [ ] **Step 2: Start the platform**

Start control (port 9090), worker (port 8080 with `--db`), gateway (port 8000). Create app, create schema + table:

```sql
CREATE TABLE "<app_id>"."users" (
  id SERIAL PRIMARY KEY,
  name TEXT NOT NULL,
  email TEXT NOT NULL UNIQUE,
  role TEXT DEFAULT 'user',
  created_at TIMESTAMPTZ DEFAULT NOW(),
  updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

- [ ] **Step 3: Deploy app that uses @appbase/db**

Create `test-app.js` that imports from the SDK (inline, since esbuild will bundle):

```javascript
import { model } from "@appbase/db";

const users = model("users", {
  name: { type: String, required: true },
  email: { type: String, required: true },
  role: { type: String, default: "user" },
});

export async function createUser(name, email) {
  return await users.create({ name, email });
}

export async function getUsers() {
  return await users.find({}).sort({ name: 1 });
}

export async function countUsers() {
  return await users.countDocuments({});
}

export async function findAdmins() {
  return await users.find({ role: "admin" }).select("name email").limit(10);
}

export async function promoteUser(id) {
  return await users.updateOne({ _id: id }, { $set: { role: "admin" } });
}
```

Deploy via `appbase deploy`.

Note: The compiler needs to resolve `@appbase/db` imports. This may require:
- Adding `sdks/db` as a dependency/symlink the compiler can find, OR
- Configuring esbuild alias in the compiler to resolve `@appbase/db` → `sdks/db/src/index.ts`

If the compiler cannot resolve the import, test with the SDK inlined directly in the app file.

- [ ] **Step 4: Call each RPC method and verify**

```bash
# createUser → should return { _id, name, email, role: "user", createdAt, updatedAt }
# getUsers → should return sorted array
# countUsers → should return number
# findAdmins → should return array with only name/email fields
# promoteUser → should return { matchedCount: 1, modifiedCount: 1 }
```

- [ ] **Step 5: Commit**

```bash
git add sdks/db/ docs/superpowers/
git commit -m "feat(sdk-db): @appbase/db SDK complete with E2E verification"
```
