import type { Todo } from "../src/types";
import { TODO_PRIORITIES, formatTodoAge, isPendingTodo, partitionTodos } from "../src/util";

const todo = (id: string, done = false): Todo => ({
  id,
  created_at: 1_700_000_000_000,
  updated_at: 1_700_000_000_000,
  created_by: null,
  updated_by: null,
  version: 1,
  userId: "usr_public",
  title: id,
  priority: "medium",
  tags: [],
  done,
  archived: false,
  deleted_at: null,
});

describe("todo utilities", () => {
  it("keeps the UI priority order stable", () => {
    expect(TODO_PRIORITIES).toEqual(["low", "medium", "high"]);
  });

  it("formats relative ages into compact labels", () => {
    const now = 1_700_000_000_000;

    expect(formatTodoAge(now, now)).toBe("now");
    expect(formatTodoAge(now - 42_000, now)).toBe("42s");
    expect(formatTodoAge(now - 7 * 60_000, now)).toBe("7m");
    expect(formatTodoAge(now - 3 * 60 * 60_000, now)).toBe("3h");
    expect(formatTodoAge(now - 2 * 24 * 60 * 60_000, now)).toBe("2d");
  });

  it("recognizes optimistic rows and partitions active before done rows", () => {
    const active = todo("todo_active");
    const done = todo("todo_done", true);
    const optimistic = todo("tmp_k1");

    expect(isPendingTodo(optimistic)).toBe(true);
    expect(isPendingTodo(active)).toBe(false);
    expect(partitionTodos([done, active, optimistic])).toEqual({
      active: [active, optimistic],
      done: [done],
    });
  });
});
