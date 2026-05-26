import { fireEvent, render, screen, within } from "@testing-library/react";
import type { Todo } from "../src/types";
import { TodoRow } from "../src/TodoRow";

const todo = (overrides: Partial<Todo> = {}): Todo => ({
  id: "todo_1",
  created_at: Date.now(),
  updated_at: Date.now(),
  created_by: null,
  updated_by: null,
  version: 1,
  userId: "usr_public",
  title: "Write component tests",
  priority: "high",
  tags: [],
  done: false,
  archived: false,
  deleted_at: null,
  ...overrides,
});

function renderRow(overrides: Partial<Todo> = {}) {
  const handlers = {
    onSetDone: vi.fn(),
    onArchive: vi.fn(),
    onDelete: vi.fn(),
  };

  const view = render(
    <ul>
      <TodoRow todo={todo(overrides)} removing={false} {...handlers} />
    </ul>,
  );

  return { ...view, handlers };
}

describe("TodoRow", () => {
  it("renders an open todo with priority styling and fires row actions", () => {
    const { handlers } = renderRow({ priority: "high" });

    const row = screen.getByRole("listitem");
    expect(row).toHaveClass("item");
    expect(row).not.toHaveClass("done");
    expect(screen.getByText("Write component tests")).toBeInTheDocument();
    expect(screen.getByTitle("high priority")).toHaveClass("pri-tick", "high");

    fireEvent.click(screen.getByRole("button", { name: "mark complete" }));
    fireEvent.click(screen.getByRole("button", { name: "archive" }));
    fireEvent.click(screen.getByRole("button", { name: "delete" }));

    expect(handlers.onSetDone).toHaveBeenCalledWith(true);
    expect(handlers.onArchive).toHaveBeenCalledTimes(1);
    expect(handlers.onDelete).toHaveBeenCalledTimes(1);
  });

  it("renders completed and optimistic-pending states distinctly", () => {
    const { unmount: unmountDone } = renderRow({ done: true, priority: "medium" });

    expect(screen.getByRole("listitem")).toHaveClass("done");
    expect(screen.getByRole("button", { name: "mark todo" })).toBeEnabled();
    expect(screen.getByTitle("medium priority")).toHaveClass("medium");
    unmountDone();

    const { unmount } = render(
      <ul>
        <TodoRow
          todo={todo({ id: "tmp_k1", title: "Optimistic row", priority: "low" })}
          removing={false}
          onSetDone={vi.fn()}
          onArchive={vi.fn()}
          onDelete={vi.fn()}
        />
      </ul>,
    );

    const row = screen.getByText("Optimistic row").closest(".item");
    expect(row).toHaveClass("pending", "entering");
    expect(within(row as HTMLElement).getByRole("button", { name: "mark complete" })).toBeDisabled();
    expect(within(row as HTMLElement).getByRole("button", { name: "archive" })).toBeDisabled();
    expect(within(row as HTMLElement).getByRole("button", { name: "delete" })).toBeDisabled();
    expect(within(row as HTMLElement).getByText("…")).toBeInTheDocument();
    unmount();
  });
});
