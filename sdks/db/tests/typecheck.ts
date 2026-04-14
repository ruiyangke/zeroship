/**
 * Type-level tests — these don't run, they just compile.
 * If this file compiles, the types are correct.
 * Run: npx tsc --project tsconfig.test.json
 */

import { createDb, t, schema } from "../src/index.js";

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

console.log("All type checks passed!");
