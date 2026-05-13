/**
 * Type-level tests — these don't run, they just compile.
 * If this file compiles, the types are correct.
 * Run: npx tsc --project tsconfig.test.json
 */

import { createDb, t, schema } from "../src/index.js";
import type { Id } from "../src/types.js";

// === Mongoose-style schema ===

const db = createDb({
  users: {
    name: { type: String, required: true },
    email: { type: String, required: true },
    age: { type: Number },
    role: { type: String },
    tags: { type: [String] },
    active: Boolean,
  },
  posts: {
    title: { type: String, required: true },
    views: { type: Number },
  },
});

// === TypeBuilder-style schema ===

const db2 = createDb({
  accounts: {
    username: t.string().required(),
    bio: t.string(),
    score: t.number(),
    verified: t.boolean(),
    friends: t.array(t.string()),
  },
});

// --- CreateInput: required fields enforced (Mongoose) ---

// ✓ valid
db.users.create({ name: "Alice", email: "a@b.com" });

// ✓ with optional fields
db.users.create({ name: "Alice", email: "a@b.com", age: 30, role: "admin" });

// @ts-expect-error — missing required field 'email'
db.users.create({ name: "Alice" });

// @ts-expect-error — missing required field 'name'
db.users.create({ email: "a@b.com" });

// --- CreateInput: required fields enforced (TypeBuilder) ---

// ✓ valid
db2.accounts.create({ username: "alice" });

// @ts-expect-error — missing required field 'username'
db2.accounts.create({ bio: "hello" });

// --- CreateInput: auto-generated fields rejected ---

// @ts-expect-error — id is auto-generated
db.users.create({ name: "Alice", email: "a@b.com", id: 1 });

// @ts-expect-error — createdAt is auto-generated
db.users.create({ name: "Alice", email: "a@b.com", createdAt: 123 });

// --- Document: result types ---

async function testResult() {
  const { data, error } = await db.users.create({ name: "Alice", email: "a@b.com" });
  if (data) {
    const name: string = data.name;        // ✓ string (required)
    const id: number = data.id;            // ✓ number (auto)
    const ts: number = data.createdAt;     // ✓ number (auto)
    const age: number | undefined = data.age; // ✓ optional

    // @ts-expect-error — typo field doesn't exist
    const x = data.typo;
  }
}

// --- TypeBuilder Document types ---

async function testBuilderResult() {
  const { data } = await db2.accounts.create({ username: "alice" });
  if (data) {
    const username: string = data.username;     // ✓ string (required)
    const bio: string | undefined = data.bio;   // ✓ optional string
    const score: number | undefined = data.score; // ✓ optional number
    const friends: string[] | undefined = data.friends; // ✓ optional array

    // @ts-expect-error — typo field doesn't exist
    const x = data.nope;
  }
}

// --- Filter: typed operators ---

// ✓ direct value
db.users.findOne({ name: "Alice" });

// ✓ comparison operators on number
db.users.find({ age: { $gt: 18, $lt: 65 } });

// ✓ string operators
db.users.find({ name: { $like: "%alice%" } });
db.users.find({ email: { $ilike: "%@gmail.com" } });

// ✓ inclusion
db.users.find({ role: { $in: ["admin", "moderator"] } });

// ✓ logical operators
db.users.find({ $and: [{ name: "Alice" }, { age: { $gte: 18 } }] });
db.users.find({ $or: [{ role: "admin" }, { age: { $gt: 21 } }] });

// ✓ null check
db.users.findOne({ age: null });

// ✓ auto-generated fields in filter
db.users.find({ id: 1 });
db.users.find({ createdAt: { $gt: Date.now() - 86400000 } });

// --- UpdateExpression: typed operators ---

// ✓ direct set
db.users.updateOne({ id: 1 }, { name: "Bob" });

// ✓ $inc on number field
db.posts.updateOne({ id: 1 }, { views: { $inc: 1 } });

// ✓ Mongoose top-level operators
db.posts.updateOne({ id: 1 }, { $inc: { views: 1 } });
db.users.updateOne({ id: 1 }, { $set: { name: "Bob", role: "admin" } });

// --- Transaction: typed tx ---

async function testTransaction() {
  const { data, error } = await db.transaction(async (tx) => {
    // tx.users is typed
    const user = await tx.users.create({ name: "Alice", email: "a@b.com" });
    const name: string = user.name;  // ✓

    // tx.posts is typed
    await tx.posts.updateOne({ id: 1 }, { views: { $inc: 1 } });

    // @ts-expect-error — tx.nonexistent doesn't exist
    await tx.nonexistent.create({});

    return user;
  });
}

// --- TxQuery: await returns typed documents ---

async function testTxQuery() {
  await db.transaction(async (tx) => {
    const users = await tx.users.find({ role: "admin" }).sort("-createdAt").limit(10);
    const name: string = users[0].name; // ✓ typed
    return users;
  });
}

// --- findById ---
db.users.findById(1);

// --- exists ---
db.users.exists({ role: "admin" });

// --- distinct: typed field name ---
db.users.distinct("name");
db.users.distinct("email");

// @ts-expect-error — nonexistent field
db.users.distinct("nonexistent");

// --- select() narrowing ---

async function testSelectNarrowing() {
  // Typed array literal narrows the result
  const { data } = await db.users.find({}).select(["name", "email"]);
  if (data) {
    const name: string = data[0].name;     // narrowed to Pick<Document, "name" | "email">
    const email: string = data[0].email;   // narrowed

    // @ts-expect-error — age is not in the selected fields
    const age = data[0].age;
  }
}

async function testSelectSingleField() {
  const { data } = await db.users.find({}).select(["id"]);
  if (data) {
    const id: number = data[0].id;   // narrowed to Pick<Document, "id">

    // @ts-expect-error — name is not in the selected fields
    const name = data[0].name;
  }
}

async function testSelectStringDoesNotNarrow() {
  // String overload should NOT narrow — returns full Document
  const { data } = await db.users.find({}).select("name email");
  if (data) {
    const name: string = data[0].name;   // still full Document
    const age: number | undefined = data[0].age;  // all fields accessible
  }
}

async function testSelectObjectDoesNotNarrow() {
  // Object overload should NOT narrow — returns full Document
  const { data } = await db.users.find({}).select({ name: 1, email: 1 });
  if (data) {
    const name: string = data[0].name;   // still full Document
    const age: number | undefined = data[0].age;  // all fields accessible
  }
}

// --- select() narrowing in transactions ---

async function testTxSelectNarrowing() {
  await db.transaction(async (tx) => {
    const users = await tx.users.find({}).select(["name", "email"]);
    const name: string = users[0].name;   // narrowed
    const email: string = users[0].email; // narrowed

    // @ts-expect-error — age is not in the selected fields
    const age = users[0].age;
    return users;
  });
}

// --- per-collection soft delete via schema().softDelete() ---

const db3 = createDb({
  articles: schema({
    title: t.string().required(),
    body: t.string(),
  }).softDelete(),
  logs: {
    message: t.string().required(),
  },
});

async function testForceDelete() {
  const { data: d1 } = await db3.articles.forceDelete({ title: "old" });
  if (d1) {
    const count: number = d1.deletedCount; // typed
  }
  const { data: d2 } = await db3.articles.forceDeleteMany({});
  if (d2) {
    const count: number = d2.deletedCount; // typed
  }
}

// --- B2: typed cross-table relations ---

// `t.ref("users")` produces TypeBuilder<Id<"users">>; `Id<"users">` and
// `Id<"posts">` are mutually incompatible brand types so accidental
// cross-table assignment is a compile error.

function testIdBrandIncompatibility() {
  const userId = 1 as Id<"users">;
  const postId = 1 as Id<"posts">;

  // ✓ assigning Id<"users"> back to Id<"users"> is fine
  const u2: Id<"users"> = userId;

  // @ts-expect-error — Id<"posts"> is not assignable to Id<"users">
  const u3: Id<"users"> = postId;

  // Discard so noUnusedLocals doesn't complain.
  void u2;
  void u3;
}

function testRefBuilderType() {
  // The builder factory preserves the literal-string type parameter so
  // `t.ref("users")` yields `TypeBuilder<Id<"users">>`. We just exercise
  // the builder here; the structural check happens inside createDb.
  const dbRefs = createDb({
    users: { name: t.string().required() },
    posts: { title: t.string().required(), authorId: t.ref("users") },
  });
  // The collection has create + find typed via Document<S> + Id<"users">.
  // We don't assert Document<S>.id is Id<"users"> here (scope reduction —
  // Document<S> still types id as number in this PR).
  void dbRefs;
}

// Runtime-only validation: t.ref("does_not_exist") throws at module-init.
// This is exercised in db.test.ts; the type system permits the call
// through because we don't enforce Tables<S> at compile time in this PR.

void testIdBrandIncompatibility;
void testRefBuilderType;

// === Tier D type-level checks ===

// D2 — t.object() with nested optional/required keys.
const dbObjects = createDb({
  profiles: {
    name: t.string().required(),
    profile: t.object({
      bio: t.string().max(500),
      social: t.object({
        twitter: t.string(),
        github: t.string().required(),
      }),
    }),
  },
});

async function testObjectInference() {
  const { data } = await dbObjects.profiles.create({
    name: "alice",
    profile: { bio: "hi", social: { github: "alice" } },
  });
  if (data) {
    // ✓ nested optional string
    const bio: string | undefined = data.profile?.bio;
    // ✓ nested required string within optional parent
    const github: string | undefined = data.profile?.social?.github;
    void bio;
    void github;
  }
}
void testObjectInference;

// D3 — t.calendarDate() infers as string at the TS layer.
const dbCal = createDb({
  events: {
    name: t.string().required(),
    eventDate: t.calendarDate(),
  },
});
async function testCalendarInference() {
  const { data } = await dbCal.events.create({ name: "x", eventDate: "2026-05-15" });
  if (data) {
    const d: string | undefined = data.eventDate;
    void d;
  }
}
void testCalendarInference;

// D4 — schema(...).withVersioning(): `version` may appear in filters and
// in `Document<S>` (typed as optional number). We exercise the API shape.
const dbVer = createDb({
  posts: schema({
    title: t.string().required(),
  }).withVersioning(),
});
async function testVersionInference() {
  await dbVer.posts.updateOne({ id: 1, version: 7 }, { title: "x" });
  const { data } = await dbVer.posts.findOne({ id: 1 });
  if (data) {
    const v: number | undefined = data.version;
    void v;
  }
}
void testVersionInference;

// === C2 — discriminated union document shapes (Phase 7) ===
//
// `events: t.union(...)` declares a collection whose row shape is a
// discriminated union. Narrowing on the discriminator key must compile
// without explicit type assertions.

const dbEvents = createDb({
  users: { name: t.string().required() },
  events: t.union(
    t.object({ kind: t.literal("login"), userId: t.ref("users"), ip: t.string().required() }),
    t.object({ kind: t.literal("error"), message: t.string().required(), stack: t.string() }),
    t.object({ kind: t.literal("metric"), name: t.string().required(), value: t.number().required() }),
  ),
});

async function testUnionNarrowing() {
  const { data } = await dbEvents.events.findOne({});
  if (!data) return;

  // Narrowing on the discriminator key — TypeScript must filter the
  // union members to the matching variant inside each branch.
  if (data.kind === "login") {
    // ✓ variant fields visible
    const uid: Id<"users"> | undefined = data.userId;
    const ip: string | undefined = data.ip;
    void uid;
    void ip;

    // @ts-expect-error — `message` is not on the "login" variant
    const m = data.message;
    void m;
  } else if (data.kind === "error") {
    const msg: string | undefined = data.message;
    void msg;

    // @ts-expect-error — `userId` is not on the "error" variant
    const u = data.userId;
    void u;
  } else if (data.kind === "metric") {
    const name: string | undefined = data.name;
    const value: number | undefined = data.value;
    void name;
    void value;
  }
}
void testUnionNarrowing;

// === Phase 7 — t.literal type-level brand ===
//
// A literal field's inferred type is the literal value, not the
// underlying primitive — so a `kind: t.literal("login")` field is
// typed as `kind: "login"`, NOT `kind: string`.
const litKind = t.literal("login");
type LitType = typeof litKind extends { _type: infer U } ? U : never;
// The brand carries the literal type.
const lk: LitType = "login";
// @ts-expect-error — "logout" is not assignable to "login"
const lkBad: LitType = "logout";
void lk;
void lkBad;

console.log("All type checks passed!");
