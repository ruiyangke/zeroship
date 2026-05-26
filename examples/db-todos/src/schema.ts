import {
  schema,
  t,
  type RowInputOf,
  type RowOf,
} from "@zeroship/db";

export const dbSchema = {
  users: {
    email:  t.string().required().unique(),
    name:   t.string().required().max(100),
    handle: t.string().required().unique().pattern(/^[a-z0-9_]+$/),
  },

  todos: schema({
    userId:   t.ref("users").required(),
    title:    t.string().required().min(1).max(200),
    priority: t.string().enum("low", "medium", "high").default("medium"),
    tags:     t.array(t.string()).default([]),
    done:     t.boolean().default(false),
    archived: t.boolean().default(false),
  }),
};

export default dbSchema;

export type User = RowOf<typeof dbSchema.users>;
export type Todo = RowOf<typeof dbSchema.todos>;
export type Priority = Todo["priority"];
export type SeedUserInput = RowInputOf<typeof dbSchema.users>;
export type TodoSnapshot = { kind: "snapshot"; rows: Todo[] };
