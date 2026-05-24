/**
 * Type-level tests for `@zeroship/server` — these don't run, they
 * just compile. If `tsc --noEmit -p tsconfig.test.json` accepts this
 * file the types are correct.
 *
 * Coverage:
 *   - B3 capability ctx separation (QueryCtx / MutationCtx / ActionCtx)
 *   - `query()` cannot write to ctx.db
 *   - `mutation()` cannot call ctx.fetch
 *   - `action()` allows both
 *   - `procedure()` keeps its broad signature (backwards compat)
 */

import {
  procedure,
  query,
  mutation,
  action,
  stream,
  subscription,
  type QueryCtx,
  type MutationCtx,
  type ActionCtx,
  type ReadOnlyCollection,
} from "../src/index.js";

// ── B3 / typecheck.b3_query_cannot_write ─────────────────────────────
//
// A `query` handler may read but must not write. The capability ctx
// `QueryCtx.db.<col>` is a `ReadOnlyCollection<unknown>` — no
// `create`, `update`, `delete`, `upsert`.

// ✓ reads compile
export const listUsers = query<unknown, unknown[]>(async (_args, ctx) => {
  const { data: u } = await ctx.db.users.get({ id: 1 });
  await ctx.db.users.countDocuments({});
  await ctx.db.users.exists({ id: 1 });
  return [u];
});

// ✓ runQuery compiles
export const fanout = query<unknown, unknown>(async (args, ctx) => {
  return await ctx.runQuery("listUsers", args);
});

export const queryCannotCreate = query<unknown, unknown>(async (_args, ctx) => {
  // @ts-expect-error — ctx.db.users.create is not on ReadOnlyCollection
  await ctx.db.users.create({ name: "x" });
  return null;
});

export const queryCannotUpdate = query<unknown, unknown>(async (_args, ctx) => {
  // @ts-expect-error — ctx.db.users.updateOne is not on ReadOnlyCollection
  await ctx.db.users.updateOne({ id: 1 }, { $set: { name: "x" } });
  return null;
});

export const queryCannotDelete = query<unknown, unknown>(async (_args, ctx) => {
  // @ts-expect-error — ctx.db.users.deleteOne is not on ReadOnlyCollection
  await ctx.db.users.deleteOne({ id: 1 });
  return null;
});

export const queryCannotFetch = query<unknown, unknown>(async (_args, ctx) => {
  // @ts-expect-error — ctx.fetch does not exist on QueryCtx
  await ctx.fetch("https://example.com");
  return null;
});

export const queryCannotRunMutation = query<unknown, unknown>(
  async (_args, ctx) => {
    // @ts-expect-error — ctx.runMutation does not exist on QueryCtx
    await ctx.runMutation("createPost", {});
    return null;
  },
);

// ── B3 / typecheck.b3_mutation_cannot_fetch ──────────────────────────

// ✓ ctx.db is present
export const createUser = mutation<{ name: string }, unknown>(
  async (_args, ctx) => {
    // ctx.db is MutationCtxDb (Record<string, unknown>). User code
    // narrows when paired with the concrete Db<T> from the
    // `export default { schema }` convention (via env.db / the
    // zeroship-schema tsconfig path), but for type-only enforcement
    // we just verify it's *present*.
    return ctx.db.users;
  },
);

// ✓ runQuery compiles
export const mutateAfterRead = mutation<unknown, unknown>(async (args, ctx) => {
  return await ctx.runQuery("listUsers", args);
});

export const mutationCannotFetch = mutation<unknown, unknown>(
  async (_args, ctx) => {
    // @ts-expect-error — ctx.fetch does not exist on MutationCtx
    await ctx.fetch("https://example.com");
    return null;
  },
);

export const mutationCannotNest = mutation<unknown, unknown>(
  async (_args, ctx) => {
    // @ts-expect-error — ctx.runMutation does not exist on MutationCtx
    //                    (mutations are already atomic — nesting is a footgun)
    await ctx.runMutation("other", {});
    return null;
  },
);

// ── B3 / typecheck.b3_action_can_fetch_and_runMutation ───────────────

// ✓ action is the most permissive — all three operations compile.
export const sendEmail = action<{ userId: number }, void>(async (args, ctx) => {
  const user = await ctx.runQuery("getUser", { id: args.userId });
  const res = await ctx.fetch("https://email-svc.example/send", {
    method: "POST",
    body: JSON.stringify({ user }),
  });
  await res.text();
  await ctx.runMutation("recordEmailSent", { userId: args.userId });
});

export const actionCannotDirectDb = action<unknown, unknown>(
  async (_args, ctx) => {
    // @ts-expect-error — ActionCtx does NOT expose direct `ctx.db`.
    //                    Actions compose DB ops via `runMutation`/`runQuery`
    //                    so each step gets its own tx.
    return ctx.db;
  },
);

// ── B3 / typecheck.procedure_backwards_compat ────────────────────────
//
// `procedure(...)` keeps its broad Handler signature. Existing user
// code that didn't take a `ctx` arg, or used an untyped `ctx`, MUST
// still compile.

// ✓ no ctx
export const greet = procedure(async (name: string) => `hi ${name}`);

// ✓ untyped ctx
export const greetWithCtx = procedure(
  async (name: string, ctx: unknown) => `hi ${name} (${typeof ctx})`,
);

// ✓ user-typed ctx with whatever they want — procedure's broad type
//   doesn't constrain it. Capability semantics map to action at the
//   manifest layer.
export const flexible = procedure(
  async (n: number, ctx: { fetch?: typeof fetch; db?: unknown }) => {
    if (ctx.fetch) await ctx.fetch("https://x");
    return n;
  },
);

// ── stream / subscription keep working ───────────────────────────────

export const tick = stream(async function* () {
  yield 1;
  yield 2;
});

export const events = subscription(async function* () {
  yield { kind: "ping" };
});

// ── Capability type re-exports surface from index ────────────────────

type QCheck = QueryCtx;
type MCheck = MutationCtx;
type ACheck = ActionCtx;
type RCheck = ReadOnlyCollection<{ id: number; name: string }>;

// Keep references so unused-type lint doesn't strip them.
export type B3Surface = QCheck | MCheck | ACheck | RCheck;
