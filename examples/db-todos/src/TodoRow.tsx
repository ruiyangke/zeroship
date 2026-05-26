import type { Todo } from "./types";
import { formatTodoAge, isPendingTodo } from "./util";

const Check = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <path
      d="M5 12.5l4.5 4.5L19 7.5"
      stroke="currentColor"
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

const ArchiveIcon = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <rect x="3.5" y="4.5" width="17" height="4" rx="1.2" stroke="currentColor" strokeWidth="1.7" />
    <path
      d="M5 8.5V18a1.5 1.5 0 0 0 1.5 1.5h11A1.5 1.5 0 0 0 19 18V8.5"
      stroke="currentColor"
      strokeWidth="1.7"
    />
    <path d="M10 12h4" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" />
  </svg>
);

const TrashIcon = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <path
      d="M4.5 6.5h15M9 6.5V5a1.5 1.5 0 0 1 1.5-1.5h3A1.5 1.5 0 0 1 15 5v1.5M7 6.5 7.7 19a1.5 1.5 0 0 0 1.5 1.4h5.6a1.5 1.5 0 0 0 1.5-1.4L17 6.5"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

export type TodoRowProps = {
  todo: Todo;
  index: number;
  removing: boolean;
  onSetDone: (done: boolean) => void;
  onArchive: () => void;
  onDelete: () => void;
};

export function TodoRow({
  todo,
  index,
  removing,
  onSetDone,
  onArchive,
  onDelete,
}: TodoRowProps) {
  const pending = isPendingTodo(todo);

  return (
    <li
      className={`item ${todo.done ? "done" : ""} ${removing ? "leaving" : ""} ${pending ? "pending" : ""}`}
      style={{ animationDelay: `${Math.min(index, 14) * 28}ms` }}
    >
      <button
        className="box"
        aria-label={todo.done ? "mark todo" : "mark complete"}
        onClick={() => onSetDone(!todo.done)}
        disabled={pending}
      >
        <Check />
      </button>

      <span className={`pri-tick ${todo.priority}`} title={`${todo.priority} priority`} aria-hidden />
      <span className="label">{todo.title}</span>

      <div className="right">
        <span className="when">{pending ? "…" : formatTodoAge(todo.created_at)}</span>
        <div className="actions">
          <button className="icon-btn" onClick={onArchive} disabled={pending} aria-label="archive" title="Archive">
            <ArchiveIcon />
          </button>
          <button className="icon-btn danger" onClick={onDelete} disabled={pending} aria-label="delete" title="Delete">
            <TrashIcon />
          </button>
        </div>
      </div>
    </li>
  );
}
