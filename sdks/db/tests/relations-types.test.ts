import { t, type Query } from "@zeroship/db";
import { installSchemaForTest } from "./_install-helper.js";
import type { NativeDb } from "../src/native.js";

const db = installSchemaForTest({
  users: { email: t.string().required(), name: t.string().required() },
  projects: { name: t.string().required() },
  todos: {
    userId: t.ref("users", { relation: "user" }), projectId: t.ref("projects", { relation: "project" }).required(),
    optionalReviewer: t.ref("users", { relation: "reviewer" }).nullable().default(null).required(),
    unnamedUser: t.ref("users"), title: t.string().required(),
  },
}, { native: { collection: () => ({ find: async () => [] }) } as unknown as NativeDb });

async function relationContracts(): Promise<void> {
  const inline = await db.todos.find({}, { with: { user: true } });
  if (inline.data) {
    const fk: string | undefined = inline.data[0].userId;
    const email: string | undefined = inline.data[0].user?.email;
    void [fk, email];
  }
  const chained = await db.todos.find().with({ user: true }).with({ project: true });
  if (chained.data) {
    const name: string | undefined = chained.data[0].project?.name;
    const email: string | undefined = chained.data[0].user?.email;
    void [name, email];
  }
  const selected = await db.todos.find().with({ user: true }).select(["id"]);
  const selectedFirst = await db.todos.find().select(["id"]).with({ user: true });
  for (const result of [selected, selectedFirst]) {
    if (result.data) {
      const email: string | undefined = result.data[0].user?.email;
      // @ts-expect-error Selecting id excludes unselected scalar fields.
      result.data[0].title;
      void email;
    }
  }
  const single = await db.todos.get("todo_a", { select: ["id"], with: { user: true } });
  if (single.data) {
    const email: string | undefined = single.data.user?.email;
    // @ts-expect-error get selection excludes unselected scalar fields.
    single.data.title;
    void email;
  }
  const page = await db.todos.find().with({ user: true }).paginate({ numItems: 10 });
  if (page.data) {
    const email: string | undefined = page.data.page[0].user?.email;
    void email;
  }
  await db.transaction(async tx => {
    const rows = await tx.todos.find({}, { with: { user: true } });
    const selected = await tx.todos.find().with({ user: true }).select(["id"]);
    const single = await tx.todos.get("todo_a", { select: ["id"], with: { user: true } });
    const page = await tx.todos.find().with({ user: true }).paginate({ numItems: 10 });
    const emails: (string | undefined)[] = [rows[0].user?.email, selected[0].user?.email, single?.user?.email, page.page[0].user?.email];
    // @ts-expect-error Transaction selection excludes unselected scalar fields.
    selected[0].title;
    // @ts-expect-error Transaction get selection excludes unselected scalar fields.
    single?.title;
    // @ts-expect-error Aliases cannot replace declared scalar columns.
    tx.todos.find().with({ userId: { field: "userId" } });
    return emails;
  });
  db.todos.find().with({ reviewer: true });
  // @ts-expect-error Foreign-key field names are not relation names.
  db.todos.find().with({ userId: true });
  // @ts-expect-error Unnamed references are not loadable edges.
  db.todos.find().with({ unnamedUser: true });
  // @ts-expect-error Unknown relation names are rejected.
  db.todos.find().with({ missing: true });
  // @ts-expect-error Arbitrary query aliases are unavailable.
  db.todos.find().with({ custom: { field: "userId" } });
  // @ts-expect-error True is the only supported relation option.
  db.todos.find().with({ user: { field: "userId" } });
  // @ts-expect-error A provided relation option must be true.
  db.todos.find().with({ user: undefined });
  // @ts-expect-error False does not load a relation.
  db.todos.find().with({ user: false });
  // @ts-expect-error Scalar fields are unavailable as relation names.
  db.todos.find().select(["id"]).with({ title: true });
  // @ts-expect-error Prototype-sensitive relation names are unavailable.
  db.todos.find().with({ constructor: true });
  const extra = { user: true, missing: true } as const;
  // @ts-expect-error Extra relation names are rejected for variables too.
  db.todos.find().with(extra);
  // @ts-expect-error An empty relation specification has no effect.
  db.todos.get("todo_a", { with: {} });
  // @ts-expect-error Relation names are not physical projection columns.
  db.todos.find().with({ user: true }).select(["user"]);
}
void relationContracts;

function untypedRelationContracts(query: Query): void {
  query.with({ author: true });
  // @ts-expect-error Runtime output metadata names are unavailable as relations.
  query.with({ _meta: true });
}
void untypedRelationContracts;
