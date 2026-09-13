import { ValidationError, type InferRow, type InferRowInput } from "@zeroship/db";
import type { env } from "zeroship";
import type {} from "../generated/zeroship/env.db";

export type User = InferRow<typeof env.db.users>;
export type Todo = InferRow<typeof env.db.todos>;
export type Priority = "low" | "medium" | "high";
export type SeedUserInput = InferRowInput<typeof env.db.users>;
export type CreateTodoInput = {
  userId: string;
  title: string;
  priority?: Priority;
};
export type TodoSnapshot = { kind: "snapshot"; rows: Todo[] };

function invalid(path: string, message: string): never {
  throw new ValidationError({ [path]: { path, message } });
}

function objectInput(input: unknown): Record<string, unknown> {
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    invalid("input", "Expected an object");
  }
  return input as Record<string, unknown>;
}

function stringInput(value: unknown, path: string): string {
  if (typeof value !== "string") invalid(path, "Expected a string");
  return value;
}

export function parseCreateTodoInput(input: unknown): CreateTodoInput {
  const value = objectInput(input);
  const userId = stringInput(value.userId, "userId");
  const title = stringInput(value.title, "title");
  if (title.length < 1 || title.length > 200) invalid("title", "Title must contain between 1 and 200 characters");
  const priority = value.priority === undefined ? "medium" : value.priority;
  if (priority !== "low" && priority !== "medium" && priority !== "high") {
    invalid("priority", "Expected low, medium, or high priority");
  }
  return { userId, title, priority };
}

export function parseSeedUserInput(input: unknown): SeedUserInput {
  const value = objectInput(input);
  const email = stringInput(value.email, "email");
  const name = stringInput(value.name, "name");
  const handle = stringInput(value.handle, "handle");
  if (name.length > 100) invalid("name", "Name must contain at most 100 characters");
  if (!/^[a-z0-9_]+$/.test(handle)) invalid("handle", "Handle must contain lowercase letters, digits, or underscores");
  return { email, name, handle };
}
