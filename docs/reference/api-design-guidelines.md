# API Design Guidelines

Rules for designing zeroship platform APIs (`@zeroship/db`, `@zeroship/auth`, `@zeroship/storage`, `@zeroship/kv`, etc.). The goal: AI generates correct code on the first attempt, non-technical creators can read it, and there's exactly one way to do each thing.

## Core Principles

### 1. One way to do each thing

If there are two ways to accomplish something, remove one.

```javascript
// Bad: three ways to get a user
db.query("SELECT * FROM users WHERE id = $1", [id])
db.users.find({ id })
db.collection("users").where("id", id).get()

// Good: one way
await users.findOne({ id });
```

### 2. Zero setup

Import and use. No configuration, no initialization, no middleware.

```javascript
// Bad:
const pool = new Pool({ host: "localhost", port: 5432, database: "myapp" });
const app = express();
app.use(express.json());
app.use(cors());
app.use(authMiddleware());

// Good:
import { db } from "@zeroship/db";
import { auth } from "@zeroship/auth";
// Just use them. Done.
```

### 3. Everything in one file

A complete app feature should fit in one file. No config files, no directory conventions, no separate route definitions.

```javascript
// Bad: 5 files for one feature
// models/recipe.js, routes/recipe.js, controllers/recipe.js, middleware/auth.js, config/db.js

// Good: 1 file
import { model, t } from "@zeroship/db";
import { auth } from "@zeroship/auth";

const recipes = model("recipes", {
  title: t.string().required(),
  author: t.ref(users).required(),
});

export async function onRequest(request) {
  const user = await auth.verify(request);
  return Response.json(await recipes.find({ author: user.id }));
}
```

### 4. Method names are English

Read the code aloud. If it sounds like a sentence, it's right.

```javascript
// Reads as: "find one user where email is alice"
await users.findOne({ email: "alice@example.com" });

// Reads as: "update product X, increment stock" — returns the row
await products.update(id, { stock: { $inc: 1 } });

// Reads as: "delete many sessions where expires at is less than now"
await sessions.deleteMany({ expires_at: { $lt: new Date() } });

// Reads as: "count users where role is admin"
await users.count({ role: "admin" });
```

### 5. Consistent parameter order

Every method follows: `(what to match, what to do)`.

```
findOne(filter)
find(filter)
updateOne(filter, changes)
updateMany(filter, changes)
deleteOne(filter)
deleteMany(filter)
insert(data)              ← no filter needed
```

Never: `(data, filter)`, `(options, filter, data)`, or any other order.

### 6. Predictable return types

The return type should be obvious from the method name:

```
insert(doc)              → document (the created doc with id)
insertMany([docs])       → [documents]
findOne(filter)          → document | null
find(filter)             → [documents]
updateOne(filter, data)  → { updated: 0 | 1 }
updateMany(filter, data) → { updated: n }
deleteOne(filter)        → { deleted: 0 | 1 }
deleteMany(filter)       → { deleted: n }
count(filter)            → number
exists(filter)           → boolean
```

Methods that return a single doc have "One" in the name. Methods that return many don't. Mutation methods return `{ verb: count }`. Never return `undefined`, `void`, or `true/false` for mutations.

### 7. No raw strings for structured data

Strings are for text. Use builders, objects, or enums for structured input.

```javascript
// Bad: magic strings
model("users", { email: "string! unique index" });     // what does "!" mean?
db.query("SELECT * FROM users WHERE role = $1", [r]);  // SQL injection risk

// Good: structured
model("users", { email: t.string().required().unique().index() });
users.find({ role: "admin" });
```

### 8. Errors are codes, not strings

```javascript
// Bad: string matching
catch (e) {
  if (e.message.includes("duplicate key")) { ... }    // fragile
  if (e.message.match(/not found/i)) { ... }           // locale-dependent
}

// Good: error codes
catch (e) {
  if (e.code === "UNIQUE_VIOLATION") { ... }           // machine-readable
  if (e.code === "NOT_FOUND") { ... }
}
```

Every error has:
- `code` — `SCREAMING_SNAKE_CASE`, machine-readable
- `message` — human-readable, for logging/display
- `field` — which field caused it (if applicable)

### 9. No implicit state

Never rely on middleware, global variables, or request mutation. Everything is an explicit function call.

```javascript
// Bad: implicit
app.use(authMiddleware());          // sets req.user somewhere
app.get("/me", (req, res) => {
  res.json(req.user);               // where did req.user come from?
});

// Good: explicit
export async function onRequest(request) {
  const user = await auth.verify(request);   // explicit call, explicit return
  return Response.json(user);
}
```

### 10. Chain reads left-to-right

Query chains should read as a natural sequence of operations:

```javascript
// Reads as: "find recipes where category is dessert, sorted by rating descending, limit 20, paginate"
await recipes
  .find({ category: "dessert" })
  .sort({ rating: -1 })
  .limit(20)
  .paginate(20, cursor);
```

Each method in the chain narrows or transforms the query. The chain executes on `await`.

## Naming Conventions

### Methods

| Pattern | Convention | Examples |
|---|---|---|
| Fetch by id | `get` | `users.get(5)` |
| Fetch one by filter | `findOne` | `users.findOne({ email })` |
| Fetch many | `find` | `users.find({ role: "admin" }).sort('-createdAt')` |
| Insert | `insert` | `users.insert({ name: "Alice" })` |
| Insert many | `insertMany` | `users.insertMany([...])` |
| Modify one (return row) | `update` | `users.update(id, { name: "Bob" })` |
| Modify many (return counts) | `updateMany` | `users.updateMany({ old: true }, { archived: true })` |
| Remove one (return row) | `delete` | `users.delete(id)` |
| Remove many (return counts) | `deleteMany` | `users.deleteMany({ expired: true })` |
| Check existence | `exists` | `users.exists({ email })` |
| Count | `count` | `users.count({ role: "admin" })` |

`update` and `delete` accept either an `id` (shorthand for `{ id }`) or
a full filter object. They return the affected document (or `null` if
nothing matched). The `*Many` variants take a filter and return counts.

### Filter operators

All prefixed with `$`. Reads as English when you say "field is greater than N":

```javascript
{ $gt: n }         // greater than
{ $gte: n }        // greater than or equal
{ $lt: n }         // less than
{ $lte: n }        // less than or equal
{ $ne: value }     // not equal
{ $in: [...] }     // in
{ $nin: [...] }    // not in
{ $like: "..." }   // like (SQL pattern)
{ $ilike: "..." }  // case-insensitive like
{ $search: "..." } // full-text search
{ $and: [...] }    // and
{ $or: [...] }     // or
{ $not: {...} }    // not
```

### Update operators

```javascript
{ $set: value }       // set to value
{ $inc: n }           // increment by n
{ $dec: n }           // decrement by n
{ $mul: n }           // multiply by n
{ $push: value }      // append to array
{ $pull: value }      // remove from array
{ $addToSet: value }  // append if not present
```

### Error codes

`SCREAMING_SNAKE_CASE`. The code describes what went wrong:

```
VALIDATION_ERROR       — input doesn't match schema
REQUIRED_FIELD         — mandatory field missing
UNIQUE_VIOLATION       — duplicate value
FOREIGN_KEY_VIOLATION  — referenced record missing
CHECK_VIOLATION        — constraint failed
NOT_FOUND              — no matching record
TRANSACTION_FAILED     — transaction rolled back
CONNECTION_ERROR       — database unavailable
UNAUTHORIZED           — invalid or missing auth
FORBIDDEN              — authenticated but not allowed
RATE_LIMITED           — too many requests
```

## Module Naming

All platform modules use the `@zeroship/` npm scope:

```javascript
import { db, model, t } from "@zeroship/db";
import { auth } from "@zeroship/auth";
import { storage } from "@zeroship/storage";
import { kv } from "@zeroship/kv";
import { queue } from "@zeroship/queue";
import { ai } from "@zeroship/ai";
import { email } from "@zeroship/email";
```

- The module name is a single lowercase word
- Import names are short, clear nouns
- `t` is the only single-letter export (type builder, used heavily)

## Export Conventions

App entry points use well-known export names:

```javascript
// HTTP request handler (every request)
export async function onRequest(request) { ... }

// Scheduled task (cron)
export async function onSchedule(event) { ... }

// Queue message handler
export async function onMessage(message) { ... }

// WebSocket connection
export async function onWebSocket(ws) { ... }

// App initialization (runs once at cold start)
export async function onInit() { ... }
```

Each export name starts with `on` + the trigger. AI reads "onRequest" and knows this handles HTTP requests. No ambiguity.

## Anti-Patterns

### Don't: overload method signatures

```javascript
// Bad: find() does different things based on argument count
find()                    // find all
find(id)                  // find by id
find({ role: "admin" })   // find by filter
find("admin")             // find by... role? name?

// Good: separate methods
findOne({ id })           // find one by filter
find({})                  // find all
find({ role: "admin" })   // find by filter
```

### Don't: return different types from the same method

```javascript
// Bad:
users.get(id)     // returns one user (object)
users.get()       // returns all users (array)

// Good:
users.findOne({ id })   // always object | null
users.find({})          // always array
```

### Don't: require configuration before use

```javascript
// Bad:
const db = new Database();
db.configure({ host: "...", pool: 10 });
await db.connect();
// NOW you can use it

// Good:
import { db } from "@zeroship/db";
await users.find({});  // works immediately
```

### Don't: use positional boolean arguments

```javascript
// Bad: what does `true` mean?
users.find({ role: "admin" }, true, false, 20)

// Good: use named options
users.find({ role: "admin" }).sort({ name: 1 }).limit(20)
```

### Don't: mutate input

```javascript
// Bad: modifies the object you passed in
const filter = { role: "admin" };
users.find(filter);
// filter is now { role: "admin", _processed: true } — surprise!

// Good: never mutate input. Create new objects internally.
```

## Checklist for New APIs

Before shipping any new `@zeroship/*` package:

- [ ] Can AI generate correct usage from just the method name?
- [ ] Is there exactly one way to do each operation?
- [ ] Does every method name read as English?
- [ ] Are parameters in consistent order (filter, data)?
- [ ] Are return types predictable from the method name?
- [ ] Is the error code list documented?
- [ ] Does it work with zero setup (just import)?
- [ ] Does a complete feature fit in one file?
- [ ] Are there no string-based DSLs?
- [ ] Are there no implicit side effects?
