// @vitest-environment node
import { expect, expectTypeOf, it } from "vitest";
import { isTypedId, parseTypedId, typedIdFromStableSeed } from "@zeroship/server/typed-id";
import {
  parseCreateTodoInput,
  parseSeedUserInput,
  type SeedUserInput,
  type Todo,
  type User,
} from "../src/schema";
import { fixtureOwnerId } from "../tests/fixture/platform";

const userId = typedIdFromStableSeed("user", "todo-validation-user");

it("derives persisted fields and insert restrictions from the generated collections", () => {
  expectTypeOf<User["id"]>().toEqualTypeOf<string>();
  expectTypeOf<Todo["created_at"]>().toEqualTypeOf<number>();
  expectTypeOf<Todo["updated_at"]>().toEqualTypeOf<number>();
  expectTypeOf<Todo["version"]>().toEqualTypeOf<number>();
  expectTypeOf<Todo["deleted_at"]>().toMatchTypeOf<number | null | undefined>();
  const input: SeedUserInput = { email: "test@example.com", name: "Test", handle: "test" };
  // @ts-expect-error Assigned ids cannot be supplied by insert callers.
  const assignedId: SeedUserInput = { ...input, id: "user_supplied" };
  expect(assignedId.id).toBe("user_supplied");
});

it("accepts valid todo input and supplies its default priority", () => {
  expect(parseCreateTodoInput({ userId, title: "Write a test" })).toEqual({
    userId, title: "Write a test", priority: "medium",
  });
  for (const priority of ["low", "medium", "high"]) {
    expect(parseCreateTodoInput({ userId, title: "a".repeat(200), priority }).priority).toBe(priority);
  }
});

it.each([
  null, [], {},
  { userId, title: "" },
  { userId, title: "a".repeat(201) },
  { userId, title: 42 },
  { userId, title: "Task", priority: "urgent" },
  { userId, title: "Task", priority: null },
])("rejects invalid todo input: %j", (input) => {
  expect(() => parseCreateTodoInput(input)).toThrowError(expect.objectContaining({ code: "VALIDATION" }));
});

it("accepts valid user input at the name boundary", () => {
  const input = { email: "test@example.com", name: "a".repeat(100), handle: "test_123" };
  expect(parseSeedUserInput(input)).toEqual(input);
});

it.each([
  null, [], {},
  { email: null, name: "Test", handle: "test" },
  { email: "test@example.com", name: "a".repeat(101), handle: "test" },
  { email: "test@example.com", name: "Test", handle: "UPPER" },
  { email: "test@example.com", name: "Test", handle: "" },
])("rejects invalid user input: %j", (input) => {
  expect(() => parseSeedUserInput(input)).toThrowError(expect.objectContaining({ code: "VALIDATION" }));
});

it("uses a canonical platform UserId for the authenticated fixture owner", () => {
  expect(isTypedId(fixtureOwnerId, "usr")).toBe(true);
  const decoded = parseTypedId(fixtureOwnerId, "usr");
  expect(isTypedId(decoded.uuid, "usr")).toBe(false);
  expect(isTypedId(typedIdFromStableSeed("app", "owner"), "usr")).toBe(false);
});
